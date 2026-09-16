//! Linux dmabuf plumbing shared between the Wayland side (`slicer.rs`) and
//! the Vulkan side (`gpu.rs`): decoding `zwp_linux_dmabuf_feedback_v1`'s
//! format table and device ids, and wrapping a [`super::gpu::DmabufImage`]
//! as a `wl_buffer`. The protocol negotiation (binding the global, sending
//! `get_default_feedback`, deciding whether to use what it reports) stays in
//! `slicer.rs`, next to the `State` it decides for; this module only holds
//! the parsing that does not need `State` at all. Since the tranche's
//! `tranche_flags` event, a main-device tranche flagged `scanout` is kept
//! separately too, so a caller wanting a modifier the display controller
//! can flip directly does not have to guess from `formats` alone.

use std::collections::HashMap;
use std::os::fd::AsFd;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::{Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use super::gpu::DmabufImage;

/// The legacy "implicit modifier" placeholder some compositors still send
/// for backward compatibility (see the protocol's `tranche_formats` event
/// doc). Every image this module ever creates uses `gpu.rs`'s explicit,
/// `DRM_FORMAT_MODIFIER_EXT`-tiled path, so an implicit modifier is never a
/// usable candidate — dropped on sight, not treated as a real option.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// One entry of the feedback's format table: a DRM fourcc paired with a
/// modifier, 16 bytes each in the mmap'd blob the `format_table` event hands
/// over (u32 format, 4 bytes padding, u64 modifier, native endianness).
#[derive(Clone, Copy)]
struct FormatEntry {
    fourcc: u32,
    modifier: u64,
}

fn parse_format_table(bytes: &[u8]) -> Vec<FormatEntry> {
    bytes
        .chunks_exact(16)
        .filter_map(|entry| {
            let fourcc = u32::from_ne_bytes(entry[0..4].try_into().ok()?);
            let modifier = u64::from_ne_bytes(entry[8..16].try_into().ok()?);
            Some(FormatEntry { fourcc, modifier })
        })
        .collect()
}

/// Decode a `device` event payload (8 bytes, native endianness) into a
/// `dev_t`, as both `main_device` and `tranche_target_device` send it.
fn decode_dev_t(bytes: &[u8]) -> Option<u64> {
    bytes
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_ne_bytes)
}

/// Bit 0 of `tranche_flags`: a hint that the compositor may scan this
/// tranche's formats out to the display controller directly, given a buffer
/// allocated accordingly. See the protocol's `tranche_flags` enum.
const TRANCHE_FLAG_SCANOUT: u32 = 1;

/// Accumulates one `zwp_linux_dmabuf_feedback_v1` exchange: the format
/// table (mmap'd once `format_table` arrives), the main device, and — for
/// whichever tranche targets that same device — the fourcc -> modifier maps
/// the caller ultimately wants: `formats` from every main-device tranche,
/// and `scanout_formats` from the subset of those tranches whose
/// `tranche_flags` carries the `scanout` bit, kept rather than discarded so
/// a caller can prefer a modifier the display controller can flip directly.
/// Every other tranche (a device other than the one we render with) is
/// walked and discarded regardless of its flags, per the protocol's
/// "grouped by tranches of preference" ordering. Binding version 4 (this
/// module never asks for more) guarantees `main_device` arrives before any
/// tranche, so `current_tranche_device` is always known by the time a
/// `tranche_formats` event needs to test against it.
#[derive(Default)]
pub struct FeedbackCollector {
    table: Vec<FormatEntry>,
    pub main_device: Option<u64>,
    current_tranche_device: Option<u64>,
    /// Raw `tranche_flags` bits for the tranche currently being walked,
    /// reset in `tranche_done`.
    current_tranche_flags: u32,
    /// fourcc -> modifiers, in advertised order, `DRM_FORMAT_MOD_INVALID`
    /// dropped, main-device tranche(s) only.
    pub formats: HashMap<u32, Vec<u64>>,
    /// Same shape as `formats`, but only the entries from main-device
    /// tranches whose `tranche_flags` carried the `scanout` bit.
    pub scanout_formats: HashMap<u32, Vec<u64>>,
    pub done: bool,
}

impl FeedbackCollector {
    /// `zwp_linux_dmabuf_feedback_v1.format_table`: mmap it read-only and
    /// private, per the event's own contract that the compositor never
    /// mutates this file's contents after sending it.
    pub fn format_table(&mut self, fd: std::os::fd::OwnedFd, _size: u32) {
        let file = std::fs::File::from(fd);
        // Safety: the protocol's `format_table` doc guarantees the
        // compositor does not mutate this file after sending the fd, and
        // this module never writes through the mapping either.
        match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => self.table = parse_format_table(&map),
            Err(_) => self.table.clear(),
        }
    }

    pub fn main_device(&mut self, device: &[u8]) {
        self.main_device = decode_dev_t(device);
    }

    pub fn tranche_target_device(&mut self, device: &[u8]) {
        self.current_tranche_device = decode_dev_t(device);
    }

    /// `tranche_flags`: sent before `tranche_formats` for the tranche it
    /// applies to. Stored raw and consulted once `tranche_formats` arrives.
    pub fn tranche_flags(&mut self, flags: u32) {
        self.current_tranche_flags = flags;
    }

    /// `tranche_formats`: `indices` is an array of u16 (native endianness)
    /// indices into the format table, only meaningful for the main-device
    /// tranche this collector cares about.
    pub fn tranche_formats(&mut self, indices: &[u8]) {
        if self.current_tranche_device.is_none() || self.current_tranche_device != self.main_device
        {
            return;
        }
        let scanout = self.current_tranche_flags & TRANCHE_FLAG_SCANOUT != 0;
        for chunk in indices.chunks_exact(2) {
            let Ok(raw) = chunk.try_into() else { continue };
            let index = u16::from_ne_bytes(raw) as usize;
            let Some(entry) = self.table.get(index) else {
                continue;
            };
            if entry.modifier == DRM_FORMAT_MOD_INVALID {
                continue;
            }
            let list = self.formats.entry(entry.fourcc).or_default();
            if !list.contains(&entry.modifier) {
                list.push(entry.modifier);
            }
            if scanout {
                let list = self.scanout_formats.entry(entry.fourcc).or_default();
                if !list.contains(&entry.modifier) {
                    list.push(entry.modifier);
                }
            }
        }
    }

    pub fn tranche_done(&mut self) {
        self.current_tranche_device = None;
        self.current_tranche_flags = 0;
    }

    pub fn done(&mut self) {
        self.done = true;
    }
}

/// Wrap a Vulkan-exported dmabuf as a `wl_buffer`, imported immediately
/// (`create_immed`) — every caller here already knows the modifier the
/// driver chose is one the compositor advertised, so there is nothing to
/// wait on a `created`/`failed` event for. `user_data` is whatever the
/// caller's `Dispatch<WlBuffer, _>` needs to tell this buffer apart from
/// every other one — the same `(presenter, slot)` pair the shm present
/// buffers use for a presenter's buffer, or `()` for the capture buffer.
pub fn dmabuf_wl_buffer<S, U>(
    dmabuf: &ZwpLinuxDmabufV1,
    image: &DmabufImage,
    handle: &QueueHandle<S>,
    user_data: U,
) -> WlBuffer
where
    S: Dispatch<ZwpLinuxBufferParamsV1, ()> + Dispatch<WlBuffer, U> + 'static,
    U: Send + Sync + 'static,
{
    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(handle, ());
    params.add(
        image.fd.as_fd(),
        0,
        image.offset,
        image.stride,
        (image.modifier >> 32) as u32,
        image.modifier as u32,
    );
    let buffer = params.create_immed(
        image.width as i32,
        image.height as i32,
        image.fourcc,
        zwp_linux_buffer_params_v1::Flags::empty(),
        handle,
        user_data,
    );
    params.destroy();
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_bytes(entries: &[(u32, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(fourcc, modifier) in entries {
            bytes.extend_from_slice(&fourcc.to_ne_bytes());
            bytes.extend_from_slice(&0u32.to_ne_bytes());
            bytes.extend_from_slice(&modifier.to_ne_bytes());
        }
        bytes
    }

    fn indices_bytes(indices: &[u16]) -> Vec<u8> {
        indices.iter().flat_map(|i| i.to_ne_bytes()).collect()
    }

    #[test]
    fn parses_a_well_formed_table() {
        let table = parse_format_table(&table_bytes(&[(0x1234, 5), (0x5678, 6)]));
        assert_eq!(table.len(), 2);
        assert_eq!(table[0].fourcc, 0x1234);
        assert_eq!(table[0].modifier, 5);
        assert_eq!(table[1].fourcc, 0x5678);
        assert_eq!(table[1].modifier, 6);
    }

    #[test]
    fn a_short_trailing_entry_is_dropped_not_panicked_on() {
        let mut bytes = table_bytes(&[(0x1234, 5)]);
        bytes.push(0); // one stray byte, not a full 16-byte entry
        let table = parse_format_table(&bytes);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn decodes_a_dev_t() {
        let dev = (226u64 << 8) | 128;
        assert_eq!(decode_dev_t(&dev.to_ne_bytes()), Some(dev));
        assert_eq!(decode_dev_t(&[1, 2, 3]), None);
    }

    #[test]
    fn only_the_main_device_tranche_is_collected() {
        let mut collector = FeedbackCollector {
            table: vec![
                FormatEntry {
                    fourcc: 0x3432_5258,
                    modifier: 7,
                },
                FormatEntry {
                    fourcc: 0x3432_5258,
                    modifier: DRM_FORMAT_MOD_INVALID,
                },
            ],
            ..Default::default()
        };
        collector.main_device(&42u64.to_ne_bytes());

        // A render-node tranche that is not the main device: discarded.
        collector.tranche_target_device(&99u64.to_ne_bytes());
        collector.tranche_formats(&indices_bytes(&[0, 1]));
        collector.tranche_done();
        assert!(collector.formats.is_empty());

        // The main-device tranche: collected, DRM_FORMAT_MOD_INVALID dropped.
        collector.tranche_target_device(&42u64.to_ne_bytes());
        collector.tranche_formats(&indices_bytes(&[0, 1]));
        collector.tranche_done();
        assert_eq!(collector.formats.get(&0x3432_5258), Some(&vec![7]));
    }

    #[test]
    fn a_flagged_main_device_tranche_lands_in_both_maps() {
        let mut collector = FeedbackCollector {
            table: vec![FormatEntry {
                fourcc: 0x3432_5258,
                modifier: 7,
            }],
            ..Default::default()
        };
        collector.main_device(&42u64.to_ne_bytes());

        collector.tranche_target_device(&42u64.to_ne_bytes());
        collector.tranche_flags(TRANCHE_FLAG_SCANOUT);
        collector.tranche_formats(&indices_bytes(&[0]));
        collector.tranche_done();

        assert_eq!(collector.formats.get(&0x3432_5258), Some(&vec![7]));
        assert_eq!(collector.scanout_formats.get(&0x3432_5258), Some(&vec![7]));
    }

    #[test]
    fn an_unflagged_main_device_tranche_lands_only_in_formats() {
        let mut collector = FeedbackCollector {
            table: vec![FormatEntry {
                fourcc: 0x3432_5258,
                modifier: 7,
            }],
            ..Default::default()
        };
        collector.main_device(&42u64.to_ne_bytes());

        collector.tranche_target_device(&42u64.to_ne_bytes());
        collector.tranche_formats(&indices_bytes(&[0]));
        collector.tranche_done();

        assert_eq!(collector.formats.get(&0x3432_5258), Some(&vec![7]));
        assert!(collector.scanout_formats.is_empty());
    }

    #[test]
    fn a_flagged_tranche_for_another_device_lands_in_neither_map() {
        let mut collector = FeedbackCollector {
            table: vec![FormatEntry {
                fourcc: 0x3432_5258,
                modifier: 7,
            }],
            ..Default::default()
        };
        collector.main_device(&42u64.to_ne_bytes());

        collector.tranche_target_device(&99u64.to_ne_bytes());
        collector.tranche_flags(TRANCHE_FLAG_SCANOUT);
        collector.tranche_formats(&indices_bytes(&[0]));
        collector.tranche_done();

        assert!(collector.formats.is_empty());
        assert!(collector.scanout_formats.is_empty());
    }

    #[test]
    fn tranche_flags_reset_between_tranches() {
        let mut collector = FeedbackCollector {
            table: vec![FormatEntry {
                fourcc: 0x3432_5258,
                modifier: 7,
            }],
            ..Default::default()
        };
        collector.main_device(&42u64.to_ne_bytes());

        // First tranche is flagged scanout.
        collector.tranche_target_device(&42u64.to_ne_bytes());
        collector.tranche_flags(TRANCHE_FLAG_SCANOUT);
        collector.tranche_formats(&indices_bytes(&[0]));
        collector.tranche_done();
        assert_eq!(collector.scanout_formats.get(&0x3432_5258), Some(&vec![7]));

        // A second, differently-fourcc'd tranche with no flags must not
        // inherit the previous tranche's scanout flag.
        collector.table.push(FormatEntry {
            fourcc: 0x5847_4258,
            modifier: 3,
        });
        collector.tranche_target_device(&42u64.to_ne_bytes());
        collector.tranche_formats(&indices_bytes(&[1]));
        collector.tranche_done();

        assert_eq!(collector.formats.get(&0x5847_4258), Some(&vec![3]));
        assert!(!collector.scanout_formats.contains_key(&0x5847_4258));
    }

    #[test]
    fn duplicate_modifiers_across_tranche_formats_calls_are_not_repeated() {
        let mut collector = FeedbackCollector {
            table: vec![FormatEntry {
                fourcc: 1,
                modifier: 9,
            }],
            ..Default::default()
        };
        collector.main_device(&1u64.to_ne_bytes());
        collector.tranche_target_device(&1u64.to_ne_bytes());
        collector.tranche_formats(&indices_bytes(&[0]));
        collector.tranche_formats(&indices_bytes(&[0]));
        assert_eq!(collector.formats.get(&1), Some(&vec![9]));
    }
}
