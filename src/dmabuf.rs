use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use smithay_client_toolkit::dmabuf::DmabufParams;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    self, ZwpLinuxBufferParamsV1,
};

use crate::geometry::Size;

pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatModifier {
    pub format: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub enum DmabufError {
    UnknownFourcc(u32),
    NoModifiers(u32),
    NoRenderNode(u64),
    ReadDir(io::Error),
    Open(PathBuf, io::Error),
    Device(io::Error),
    Allocate {
        fourcc: u32,
        error: io::Error,
    },
    PlaneFd {
        plane: u32,
        error: gbm::InvalidFdError,
    },
    ImportFailed {
        fourcc: u32,
        modifiers: Vec<u64>,
    },
}

/// `AR24 (0x34325241)`: the fourcc as four characters, then as hex.
pub fn fourcc_text(fourcc: u32) -> String {
    let chars: String = fourcc
        .to_le_bytes()
        .iter()
        .map(|&b| char::from(b))
        .collect();
    format!("{chars} (0x{fourcc:08x})")
}

impl fmt::Display for DmabufError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmabufError::UnknownFourcc(c) => write!(f, "unknown fourcc {}", fourcc_text(*c)),
            DmabufError::NoModifiers(c) => {
                write!(f, "no modifier advertised for format {}", fourcc_text(*c))
            }
            DmabufError::NoRenderNode(d) => {
                write!(f, "no /dev/dri/renderD* node with device number 0x{d:x}")
            }
            DmabufError::ReadDir(e) => write!(f, "cannot read /dev/dri: {e}"),
            DmabufError::Open(p, e) => write!(f, "cannot open {}: {e}", p.display()),
            DmabufError::Device(e) => write!(f, "gbm_create_device failed: {e}"),
            DmabufError::Allocate { fourcc, error } => {
                write!(
                    f,
                    "gbm allocation of {} failed: {error}",
                    fourcc_text(*fourcc)
                )
            }
            DmabufError::PlaneFd { plane, error } => write!(f, "fd for plane {plane}: {error}"),
            DmabufError::ImportFailed { fourcc, modifiers } => {
                let list: Vec<String> = modifiers.iter().map(|m| format!("0x{m:016x}")).collect();
                write!(
                    f,
                    "zwp_linux_buffer_params_v1 failed for fourcc {} with modifiers [{}]",
                    fourcc_text(*fourcc),
                    list.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for DmabufError {}

/// Modifiers of `fourcc` in tranche order, then index order. Keeps the first occurrence of each
/// value, drops `DRM_FORMAT_MOD_INVALID` and skips indices outside the table.
pub fn modifiers_for(table: &[FormatModifier], tranches: &[Vec<u16>], fourcc: u32) -> Vec<u64> {
    let mut out = Vec::new();
    for &idx in tranches.iter().flatten() {
        let Some(entry) = table.get(usize::from(idx)) else {
            continue;
        };
        if entry.format == fourcc
            && entry.modifier != DRM_FORMAT_MOD_INVALID
            && !out.contains(&entry.modifier)
        {
            out.push(entry.modifier);
        }
    }
    out
}

pub fn gbm_format(fourcc: u32) -> Result<gbm::Format, DmabufError> {
    gbm::Format::try_from(fourcc).map_err(|_| DmabufError::UnknownFourcc(fourcc))
}

/// A GBM device on the render node that the compositor's dmabuf feedback names.
pub struct Allocator {
    device: gbm::Device<File>,
}

impl Allocator {
    /// Opens the `/dev/dri/renderD*` node whose device number is `main_device`, read-write.
    pub fn open(main_device: u64) -> Result<Allocator, DmabufError> {
        let entries = fs::read_dir("/dev/dri").map_err(DmabufError::ReadDir)?;
        for entry in entries {
            let entry = entry.map_err(DmabufError::ReadDir)?;
            if !entry.file_name().to_string_lossy().starts_with("renderD") {
                continue;
            }
            let path = entry.path();
            let meta = fs::metadata(&path).map_err(|e| DmabufError::Open(path.clone(), e))?;
            if meta.rdev() != main_device {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| DmabufError::Open(path.clone(), e))?;
            let device = gbm::Device::new(file).map_err(DmabufError::Device)?;
            return Ok(Allocator { device });
        }
        Err(DmabufError::NoRenderNode(main_device))
    }

    /// Allocates a renderable buffer object. The buffer objects must be dropped before `self`.
    pub fn allocate(
        &self,
        size: Size,
        fourcc: u32,
        modifiers: &[u64],
    ) -> Result<gbm::BufferObject<()>, DmabufError> {
        let format = gbm_format(fourcc)?;
        if modifiers.is_empty() {
            return Err(DmabufError::NoModifiers(fourcc));
        }
        self.device
            .create_buffer_object_with_modifiers2::<()>(
                size.width,
                size.height,
                format,
                modifiers.iter().map(|&m| gbm::Modifier::from(m)),
                gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|error| DmabufError::Allocate { fourcc, error })
    }
}

/// Sends `add` for every plane and `create`. Each plane fd is closed right after its `add`.
pub fn import(
    bo: &gbm::BufferObject<()>,
    params: DmabufParams,
) -> Result<ZwpLinuxBufferParamsV1, DmabufError> {
    for p in 0..bo.plane_count() {
        let plane = p as i32;
        let fd = bo
            .fd_for_plane(plane)
            .map_err(|error| DmabufError::PlaneFd { plane: p, error })?;
        params.add(
            fd.as_fd(),
            p,
            bo.offset(plane),
            bo.stride_for_plane(plane),
            u64::from(bo.modifier()),
        );
    }
    Ok(params.create(
        bo.width() as i32,
        bo.height() as i32,
        bo.format() as u32,
        zwp_linux_buffer_params_v1::Flags::empty(),
    ))
}

pub struct DmaBuffer {
    pub bo: gbm::BufferObject<()>,
    pub wl_buffer: WlBuffer,
}

impl DmaBuffer {
    /// Destroys the `wl_buffer`, then drops the buffer object.
    pub fn destroy(self) {
        self.wl_buffer.destroy();
        drop(self.bo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AR24: u32 = 0x34325241;
    const XR24: u32 = 0x34325258;

    const BLOCK_LINEAR: [u64; 12] = [
        0x0300000000606010,
        0x0300000000606011,
        0x0300000000606012,
        0x0300000000606013,
        0x0300000000606014,
        0x0300000000606015,
        0x0300000000e08010,
        0x0300000000e08011,
        0x0300000000e08012,
        0x0300000000e08013,
        0x0300000000e08014,
        0x0300000000e08015,
    ];

    fn table() -> Vec<FormatModifier> {
        [AR24, XR24]
            .into_iter()
            .flat_map(|format| {
                BLOCK_LINEAR
                    .into_iter()
                    .chain([DRM_FORMAT_MOD_INVALID])
                    .map(move |modifier| FormatModifier { format, modifier })
            })
            .collect()
    }

    #[test]
    fn modifiers_for_cases() {
        let all: Vec<u16> = (0..u16::try_from(table().len()).unwrap()).collect();
        let cases = vec![
            ("ar24", vec![all.clone()], AR24, BLOCK_LINEAR.to_vec()),
            ("xr24", vec![all.clone()], XR24, BLOCK_LINEAR.to_vec()),
            ("fourcc absent", vec![all.clone()], 0x30334241, vec![]),
            (
                "same modifier in two tranches",
                vec![vec![0, 1], vec![1, 2]],
                AR24,
                BLOCK_LINEAR[..3].to_vec(),
            ),
            (
                "tranche order wins over table order",
                vec![vec![3, 1], vec![2, 0]],
                AR24,
                vec![
                    BLOCK_LINEAR[3],
                    BLOCK_LINEAR[1],
                    BLOCK_LINEAR[2],
                    BLOCK_LINEAR[0],
                ],
            ),
            (
                "invalid dropped and out of range skipped",
                vec![vec![12, 99, 0]],
                AR24,
                vec![BLOCK_LINEAR[0]],
            ),
            ("no tranches", vec![], AR24, vec![]),
        ];
        for (name, tranches, fourcc, want) in cases {
            assert_eq!(modifiers_for(&table(), &tranches, fourcc), want, "{name}");
        }
    }

    #[test]
    fn error_display_cases() {
        let cases = [
            (
                "import failed",
                DmabufError::ImportFailed {
                    fourcc: AR24,
                    modifiers: vec![BLOCK_LINEAR[0], BLOCK_LINEAR[1]],
                },
                "zwp_linux_buffer_params_v1 failed for fourcc AR24 (0x34325241) with modifiers [0x0300000000606010, 0x0300000000606011]",
            ),
            (
                "import failed without modifiers",
                DmabufError::ImportFailed {
                    fourcc: XR24,
                    modifiers: vec![],
                },
                "zwp_linux_buffer_params_v1 failed for fourcc XR24 (0x34325258) with modifiers []",
            ),
        ];
        for (name, err, want) in cases {
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    #[test]
    fn gbm_format_cases() {
        let cases = [
            ("ar24", AR24, Some(gbm::Format::Argb8888)),
            ("xr24", XR24, Some(gbm::Format::Xrgb8888)),
            ("ab30", 0x30334241, Some(gbm::Format::Abgr2101010)),
            ("xb30", 0x30334258, Some(gbm::Format::Xbgr2101010)),
            ("zero", 0, None),
            ("unassigned", 0x31313131, None),
        ];
        for (name, fourcc, want) in cases {
            match (gbm_format(fourcc), want) {
                (Ok(got), Some(want)) => assert_eq!(got, want, "{name}"),
                (Err(DmabufError::UnknownFourcc(c)), None) => assert_eq!(c, fourcc, "{name}"),
                (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
            }
        }
    }
}
