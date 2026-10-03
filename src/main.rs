#![deny(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod app;
mod capture;
mod chrome;
mod cli;
mod clients;
mod config;
mod control;
mod coords;
mod dmabuf;
mod exit;
mod geometry;
mod hypr;
mod input;
mod ipc;
mod layout;
mod overlay;
mod placement;
mod protocol;
mod report;
mod surface;
#[cfg(test)]
mod testutil;
mod tray;

use std::time::{Duration, Instant};

use calloop::EventLoop;
use calloop::signals::{Signal, Signals};

use crate::app::{App, prepare, run_command, setup};
use crate::exit::{dispatch_reason, emit_or_finish, fail, failure, loop_error, remove_control};
use crate::report::{Line, Reporter};

fn main() -> ! {
    let start = Instant::now();
    let args = match cli::parse_os(std::env::args_os().skip(1)) {
        Ok(cli::Invocation::Daemon(args)) => args,
        Ok(cli::Invocation::Command(command)) => run_command(command),
        Err(e) => {
            let message = e.to_string();
            let mut reporter = Reporter::stderr(false);
            emit_or_finish(
                &mut reporter,
                &Line::Usage {
                    message: message.clone(),
                },
            );
            fail(&mut reporter, 2, message, None)
        }
    };
    let reporter = match args.log.as_deref() {
        None => Reporter::stderr(args.verbose),
        Some(path) => match Reporter::open(path, args.verbose) {
            Ok(reporter) => reporter,
            Err(e) => {
                let mut reporter = Reporter::stderr(args.verbose);
                fail(
                    &mut reporter,
                    1,
                    format!("log {}: {e}", path.display()),
                    None,
                )
            }
        },
    };
    let (mut startup, snapshot) = prepare(&args, reporter);

    let mut event_loop = match EventLoop::<App>::try_new() {
        Ok(l) => l,
        Err(e) => fail(
            &mut startup.reporter,
            1,
            loop_error(e),
            remove_control(startup.control),
        ),
    };
    let mut app = setup(startup, event_loop.handle());
    let signals = match Signals::new(&[Signal::SIGINT, Signal::SIGTERM]) {
        Ok(s) => s,
        Err(e) => app.shutdown(failure(loop_error(e))),
    };
    let inserted = event_loop
        .handle()
        .insert_source(signals, |event, _, app: &mut App| {
            let reason = match event.signal() {
                Signal::SIGINT => "signal SIGINT",
                _ => "signal SIGTERM",
            };
            app.stop_with(0, reason);
        });
    if let Err(e) = inserted {
        app.shutdown(failure(loop_error(&e)));
    }
    app.start_tray();
    for entry in &snapshot {
        if app.stop.is_some() {
            break;
        }
        let changes = app.clients.add(entry);
        app.apply_changes(changes);
    }
    if let Err(e) = app.insert_event_source() {
        app.shutdown(failure(e));
    }
    if let Err(e) = app.insert_control_source() {
        app.shutdown(failure(e));
    }
    app.watch_tray();
    app.drain_tray(false);
    app.announce_control();

    let deadline = args.seconds.map(|s| start + Duration::from_secs(s));
    let stop = loop {
        if let Some(stop) = app.stop.take() {
            break stop;
        }
        let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if let Err(e) = event_loop.dispatch(timeout, &mut app) {
            let wayland = app.conn.backend().last_error().map(|w| w.to_string());
            app.stop(failure(dispatch_reason(wayland, e.to_string())));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            app.stop_with(0, "seconds elapsed");
        }
    };
    app.shutdown(stop)
}
