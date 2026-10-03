use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;

use super::App;
use crate::capture::Input;
use crate::dmabuf;
use crate::dmabuf::{DmaBuffer, DmabufError, FormatModifier};
use crate::exit::{Stop, failure};
use crate::geometry::Size;
use crate::report::Line;

pub(super) struct Feedback {
    pub(super) table: Vec<FormatModifier>,
    pub(super) tranches: Vec<Vec<u16>>,
}

pub(super) struct PendingImport {
    bo: gbm::BufferObject<()>,
    fourcc: u32,
    modifiers: Vec<u64>,
}

/// Outcome of a `created` or `failed` event for a params object.
#[derive(Debug, PartialEq, Eq)]
enum Resolved<T> {
    Current { owner: u64, slot: usize, import: T },
    Superseded(T),
}

struct Pending<K, T> {
    owner: u64,
    slot: usize,
    key: K,
    import: T,
    current: bool,
}

/// Pending imports of every client. A replaced import, and every import of a removed client, stays
/// listed until its own event arrives.
pub(super) struct Imports<K, T> {
    entries: Vec<Pending<K, T>>,
}

impl<K: PartialEq, T> Imports<K, T> {
    pub(super) fn new() -> Self {
        Imports {
            entries: Vec::new(),
        }
    }

    fn begin(&mut self, owner: u64, slot: usize, key: K, import: T) {
        for entry in &mut self.entries {
            if entry.owner == owner && entry.slot == slot {
                entry.current = false;
            }
        }
        self.entries.push(Pending {
            owner,
            slot,
            key,
            import,
            current: true,
        });
    }

    /// Makes every import of `owner` resolve as superseded.
    pub(super) fn release_owner(&mut self, owner: u64) {
        for entry in &mut self.entries {
            if entry.owner == owner {
                entry.current = false;
            }
        }
    }

    /// Removes `key` from the list and returns its payload. `None` means the key is not listed.
    fn resolve(&mut self, key: &K) -> Option<Resolved<T>> {
        let at = self.entries.iter().position(|e| e.key == *key)?;
        let entry = self.entries.remove(at);
        Some(if entry.current {
            Resolved::Current {
                owner: entry.owner,
                slot: entry.slot,
                import: entry.import,
            }
        } else {
            Resolved::Superseded(entry.import)
        })
    }

    /// Empties the list and returns every entry, current and superseded.
    pub(super) fn drain(&mut self) -> Vec<(K, T)> {
        self.entries.drain(..).map(|e| (e.key, e.import)).collect()
    }
}

impl App {
    pub(super) fn allocate(
        &mut self,
        address: u64,
        slot: usize,
        fourcc: u32,
        size: Size,
    ) -> Result<(), Stop> {
        let Some(record) = self.records.get_mut(&address) else {
            return Ok(());
        };
        if let Some(old) = record.buffers[slot].take() {
            old.destroy();
        }
        let modifiers =
            dmabuf::modifiers_for(&self.feedback.table, &self.feedback.tranches, fourcc);
        let bo = self
            .allocator
            .allocate(size, fourcc, &modifiers)
            .map_err(failure)?;
        self.emit(&Line::Format {
            address,
            fourcc,
            modifier: u64::from(bo.modifier()),
            size,
            planes: bo.plane_count(),
        });
        if self.stop.is_some() {
            return Ok(());
        }
        let params = self.dmabuf_state.create_params(&self.qh).map_err(failure)?;
        let params = dmabuf::import(&bo, params).map_err(failure)?;
        self.imports.begin(
            address,
            slot,
            params,
            PendingImport {
                bo,
                fourcc,
                modifiers,
            },
        );
        Ok(())
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpLinuxDmabufFeedbackV1,
        _: DmabufFeedback,
    ) {
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        params: &ZwpLinuxBufferParamsV1,
        buffer: WlBuffer,
    ) {
        match self.imports.resolve(params) {
            Some(Resolved::Current {
                owner,
                slot,
                import,
            }) => {
                params.destroy();
                match self.records.get_mut(&owner) {
                    Some(record) => {
                        record.buffers[slot] = Some(DmaBuffer {
                            bo: import.bo,
                            wl_buffer: buffer,
                        });
                        self.feed(owner, Input::Imported { slot });
                    }
                    None => {
                        buffer.destroy();
                        drop(import);
                    }
                }
            }
            Some(Resolved::Superseded(import)) => {
                buffer.destroy();
                params.destroy();
                drop(import);
            }
            None => {
                buffer.destroy();
                params.destroy();
            }
        }
    }

    fn failed(&mut self, _: &Connection, _: &QueueHandle<Self>, params: &ZwpLinuxBufferParamsV1) {
        let resolved = self.imports.resolve(params);
        params.destroy();
        if let Some(Resolved::Current { import, .. }) = resolved {
            let error = DmabufError::ImportFailed {
                fourcc: import.fourcc,
                modifiers: import.modifiers,
            };
            self.stop(failure(error));
        }
    }

    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, buffer: &WlBuffer) {
        let owner = self.records.iter().find_map(|(address, record)| {
            record
                .buffers
                .iter()
                .position(|b| b.as_ref().is_some_and(|b| b.wl_buffer == *buffer))
                .map(|slot| (*address, slot))
        });
        if let Some((address, slot)) = owner {
            self.feed(address, Input::Released { slot });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    enum Op {
        Begin(u64, usize, u32, &'static str),
        Release(u64),
        Event(u32, Option<Resolved<&'static str>>),
    }

    #[test]
    fn pending_import_cases() {
        use Op::{Begin, Event, Release};
        use Resolved::{Current, Superseded};
        let cur = |owner, slot, import| {
            Some(Current {
                owner,
                slot,
                import,
            })
        };
        let cases = [
            (
                "one pending, its own event",
                vec![Begin(1, 0, 1, "a"), Event(1, cur(1, 0, "a"))],
                vec![],
            ),
            (
                "replaced once, old then new",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Event(1, Some(Superseded("a"))),
                    Event(2, cur(1, 0, "b")),
                ],
                vec![],
            ),
            (
                "replaced once, new then old",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Event(2, cur(1, 0, "b")),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "replaced twice, each old event",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(3, "c")],
            ),
            (
                "replaced twice, middle event only",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![(1, "a"), (3, "c")],
            ),
            (
                "replaced twice, oldest event resolves to its own payload",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 0, 2, "b"),
                    Begin(1, 0, 3, "c"),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![(2, "b"), (3, "c")],
            ),
            (
                "slots are independent",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 1, 2, "b"),
                    Event(2, cur(1, 1, "b")),
                ],
                vec![(1, "a")],
            ),
            (
                "owners are independent in the same slot",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(2, 0, 2, "b"),
                    Event(2, cur(2, 0, "b")),
                ],
                vec![(1, "a")],
            ),
            (
                "released owner's current import resolves as superseded",
                vec![
                    Begin(1, 0, 1, "a"),
                    Release(1),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "hidden owner's imports in both slots resolve as superseded",
                vec![
                    Begin(1, 0, 1, "a"),
                    Begin(1, 1, 2, "b"),
                    Release(1),
                    Event(1, Some(Superseded("a"))),
                    Event(2, Some(Superseded("b"))),
                ],
                vec![],
            ),
            (
                "released owner keeps every pending import",
                vec![Begin(1, 0, 1, "a"), Begin(1, 1, 2, "b"), Release(1)],
                vec![(1, "a"), (2, "b")],
            ),
            (
                "re-added owner's import is current",
                vec![
                    Begin(1, 0, 1, "a"),
                    Release(1),
                    Begin(1, 0, 2, "b"),
                    Event(2, cur(1, 0, "b")),
                    Event(1, Some(Superseded("a"))),
                ],
                vec![],
            ),
            (
                "unknown key",
                vec![Begin(1, 0, 1, "a"), Event(9, None)],
                vec![(1, "a")],
            ),
        ];
        for (name, ops, want) in cases {
            let mut imports = Imports::new();
            for op in ops {
                match op {
                    Begin(owner, slot, key, import) => imports.begin(owner, slot, key, import),
                    Release(owner) => imports.release_owner(owner),
                    Event(key, want) => assert_eq!(imports.resolve(&key), want, "{name}"),
                }
            }
            let mut left = imports.drain();
            left.sort_by_key(|(key, _)| *key);
            assert_eq!(left, want, "{name}");
        }
    }
}
