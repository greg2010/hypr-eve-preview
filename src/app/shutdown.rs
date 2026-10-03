use super::App;
use crate::exit::{Stop, finish, remove_control};

impl App {
    /// Removes the control sources and the socket, then drops the tray and the ping, and tears
    /// down timers, the pending layout save and Wayland objects. Prints `exit` and exits.
    /// Callers keep the `Signals` source installed until exit, so a late signal stays pending.
    pub(crate) fn shutdown(mut self, stop: Stop) -> ! {
        let mut teardown = Vec::new();
        for token in [
            self.control_loop.listener.take(),
            self.control_loop.pause.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.loop_handle.remove(token);
        }
        let ids: Vec<_> = self.control_loop.connections.keys().copied().collect();
        for id in ids {
            self.forget_connection(id);
        }
        if let Some(server) = self.control.take() {
            teardown.extend(remove_control(server));
        }
        for token in [self.tray_token.take(), self.ping_token.take()]
            .into_iter()
            .flatten()
        {
            self.loop_handle.remove(token);
        }
        self.ping = None;
        drop(self.tray.take());
        for record in self.records.values_mut() {
            if let Some(token) = record.timer.take() {
                self.loop_handle.remove(token);
            }
        }
        if let Some(token) = self.save_timer.take() {
            self.loop_handle.remove(token);
        }
        for (_, token) in std::mem::take(&mut self.retries) {
            self.loop_handle.remove(token);
        }
        if self.schedule.pending() {
            self.write_layout();
        }
        let mut objects = Vec::new();
        for record in std::mem::take(&mut self.records).into_values() {
            if let Some(frame) = record.frame
                && frame.got_event
            {
                frame.proxy.destroy();
            }
            for traveller in record.travellers {
                traveller.overlay.destroy();
            }
            if let Some(overlay) = record.overlay {
                overlay.destroy();
            }
            for buffer in record.buffers.into_iter().flatten() {
                buffer.wl_buffer.destroy();
                objects.push(buffer.bo);
            }
        }
        let mut pending = Vec::new();
        for (params, import) in self.imports.drain() {
            params.destroy();
            pending.push(import);
        }
        for pointer in std::mem::take(&mut self.pointers) {
            pointer.relative.destroy();
            drop(pointer.themed);
        }
        self.export_manager.destroy();
        self.alpha.destroy();
        teardown.extend(self.conn.flush().err().map(|e| format!("flush: {e}")));
        let error = (!teardown.is_empty()).then(|| teardown.join("; "));
        drop(objects);
        drop(pending);
        drop(self.allocator);
        if let Some(token) = self.event_token.take() {
            self.loop_handle.remove(token);
        }
        drop(self.events);
        let stop = if matches!(self.stop, Some(Stop::StderrFailed)) {
            Stop::StderrFailed
        } else {
            stop
        };
        finish(&mut self.reporter, stop, error)
    }
}
