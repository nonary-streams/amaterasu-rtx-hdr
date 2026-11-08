//! Resource import belongs to the effect. The SDK carries the exact source
//! descriptor and dependency, independent of the selected capture backend.
use super::*;
use amaterasu_module_sdk::frame_processing::{ProcessingFrame, RetainedProcessingInput};
const GENERATION: u64 = 1;
const NONE: FrameDescriptor = FrameDescriptor {
    kind: FRAME_SYNC_NONE,
    version: 0,
    size: 0,
    reserved: 0,
    namespace: [0; 16],
    data: std::ptr::null(),
};
#[derive(Clone, Copy)]
enum Capabilities {
    DmaBuf(DmaBufResourceCapabilities),
    Cpu(CpuResourceCapabilitiesV2),
}
struct Offer {
    id: u64,
    caps: Capabilities,
}
pub(super) struct InputOffers {
    offers: Vec<Offer>,
}
impl InputOffers {
    pub fn new() -> Self {
        let mut offers = Vec::new();
        for fourcc in [
            *b"XR24", *b"AR24", *b"XB24", *b"AB24", *b"XR30", *b"AR30", *b"XB30", *b"AB30",
        ] {
            offers.push(Offer {
                id: offers.len() as u64 + 1,
                caps: Capabilities::DmaBuf(DmaBufResourceCapabilities {
                    fourcc: u32::from_le_bytes(fourcc),
                    color: DMABUF_COLOR_SRGB,
                    modifier: DRM_FORMAT_MOD_INVALID,
                    min_width: 16,
                    max_width: 8192,
                    min_height: 16,
                    max_height: 8192,
                    device_id: [0; 16],
                }),
            });
        }
        for (format, matrix, transfer, range) in [
            (
                PIXEL_BGRA8,
                FRAME_MATRIX_RGB,
                FRAME_TRANSFER_SRGB,
                FRAME_RANGE_FULL,
            ),
            (
                PIXEL_BGRA8,
                FRAME_MATRIX_RGB,
                FRAME_TRANSFER_BT709,
                FRAME_RANGE_FULL,
            ),
            (
                PIXEL_NV12,
                FRAME_MATRIX_BT709,
                FRAME_TRANSFER_BT709,
                FRAME_RANGE_LIMITED,
            ),
            (
                PIXEL_NV12,
                FRAME_MATRIX_BT709,
                FRAME_TRANSFER_BT709,
                FRAME_RANGE_FULL,
            ),
            (
                PIXEL_P010,
                FRAME_MATRIX_BT709,
                FRAME_TRANSFER_BT709,
                FRAME_RANGE_LIMITED,
            ),
            (
                PIXEL_P010,
                FRAME_MATRIX_BT709,
                FRAME_TRANSFER_BT709,
                FRAME_RANGE_FULL,
            ),
        ] {
            offers.push(Offer {
                id: offers.len() as u64 + 1,
                caps: Capabilities::Cpu(CpuResourceCapabilitiesV2 {
                    base: CpuResourceCapabilities {
                        format,
                        min_width: 16,
                        max_width: 8192,
                        min_height: 16,
                        max_height: 8192,
                    },
                    color: FrameColorMetadata {
                        version: 1,
                        size: std::mem::size_of::<FrameColorMetadata>() as u32,
                        primaries: FRAME_PRIMARIES_BT709,
                        transfer,
                        matrix,
                        range,
                    },
                }),
            });
        }
        Self { offers }
    }
    pub fn enumerate(&self) -> (u64, Vec<CaptureResourceOffer>) {
        (
            GENERATION,
            self.offers
                .iter()
                .map(|offer| {
                    let (resource, accessibility) = match &offer.caps {
                        Capabilities::DmaBuf(caps) => (
                            FrameDescriptor {
                                kind: FRAME_RESOURCE_DMABUF,
                                version: 1,
                                size: std::mem::size_of::<DmaBufResourceCapabilities>() as u32,
                                reserved: 0,
                                namespace: [0; 16],
                                data: std::ptr::from_ref(caps).cast(),
                            },
                            FRAME_ACCESS_GPU,
                        ),
                        Capabilities::Cpu(caps) => (
                            FrameDescriptor {
                                kind: FRAME_RESOURCE_CPU,
                                version: 2,
                                size: std::mem::size_of::<CpuResourceCapabilitiesV2>() as u32,
                                reserved: 0,
                                namespace: [0; 16],
                                data: std::ptr::from_ref(caps).cast(),
                            },
                            FRAME_ACCESS_CPU,
                        ),
                    };
                    CaptureResourceOffer {
                        id: offer.id,
                        ownership: FRAME_OWNERSHIP_RETAINABLE,
                        accessibility,
                        reserved: 0,
                        resource,
                        acquire_sync: NONE,
                        completion_sync: if resource.kind == FRAME_RESOURCE_DMABUF {
                            FrameDescriptor {
                                kind: FRAME_SYNC_SYNC_FILE,
                                version: 1,
                                ..NONE
                            }
                        } else {
                            NONE
                        },
                    }
                })
                .collect(),
        )
    }
    pub fn select(
        &self,
        generation: u64,
        id: u64,
        source: &CaptureResourceOffer,
    ) -> Result<SelectedInput, Status> {
        if generation != GENERATION
            || source.ownership != FRAME_OWNERSHIP_RETAINABLE
            || source.reserved != 0
            || source.resource.reserved != 0
            || source.resource.namespace != [0; 16]
            || source.resource.data.is_null()
            || source.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
            || !none(&source.acquire_sync)
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        let own = self
            .offers
            .iter()
            .find(|offer| offer.id == id)
            .ok_or(MEDIA_UNSUPPORTED)?;
        let caps = match own.caps {
            Capabilities::DmaBuf(wanted)
                if source.resource.kind == FRAME_RESOURCE_DMABUF
                    && source.resource.version == 1
                    && source.resource.size as usize
                        >= std::mem::size_of::<DmaBufResourceCapabilities>() =>
            {
                // SAFETY: native offer lends the complete standard capabilities.
                let mut actual = unsafe {
                    source
                        .resource
                        .data
                        .cast::<DmaBufResourceCapabilities>()
                        .read_unaligned()
                };
                if source.accessibility & FRAME_ACCESS_GPU == 0
                    || source.completion_sync.kind != FRAME_SYNC_SYNC_FILE
                    || source.completion_sync.version != 1
                    || source.completion_sync.size != 0
                    || source.completion_sync.reserved != 0
                    || source.completion_sync.namespace != [0; 16]
                    || !source.completion_sync.data.is_null()
                    || actual.fourcc != wanted.fourcc
                    || actual.color != wanted.color
                {
                    return Err(MEDIA_UNSUPPORTED);
                }
                actual.min_width = actual.min_width.max(wanted.min_width);
                actual.min_height = actual.min_height.max(wanted.min_height);
                actual.max_width = actual.max_width.min(wanted.max_width);
                actual.max_height = actual.max_height.min(wanted.max_height);
                if actual.min_width > actual.max_width || actual.min_height > actual.max_height {
                    return Err(MEDIA_UNSUPPORTED);
                }
                Capabilities::DmaBuf(actual)
            }
            Capabilities::Cpu(wanted)
                if source.resource.kind == FRAME_RESOURCE_CPU
                    && source.resource.version == 2
                    && source.resource.size as usize
                        >= std::mem::size_of::<CpuResourceCapabilitiesV2>() =>
            {
                let mut actual = unsafe {
                    source
                        .resource
                        .data
                        .cast::<CpuResourceCapabilitiesV2>()
                        .read_unaligned()
                };
                if source.accessibility & FRAME_ACCESS_CPU == 0
                    || !none(&source.completion_sync)
                    || actual.base.format != wanted.base.format
                    || color(actual.color) != color(wanted.color)
                {
                    return Err(MEDIA_UNSUPPORTED);
                }
                actual.base.min_width = actual.base.min_width.max(wanted.base.min_width);
                actual.base.min_height = actual.base.min_height.max(wanted.base.min_height);
                actual.base.max_width = actual.base.max_width.min(wanted.base.max_width);
                actual.base.max_height = actual.base.max_height.min(wanted.base.max_height);
                if actual.base.min_width > actual.base.max_width
                    || actual.base.min_height > actual.base.max_height
                {
                    return Err(MEDIA_UNSUPPORTED);
                }
                Capabilities::Cpu(actual)
            }
            _ => return Err(MEDIA_UNSUPPORTED),
        };
        Ok(SelectedInput { caps })
    }
}
fn none(d: &FrameDescriptor) -> bool {
    d.kind == FRAME_SYNC_NONE
        && d.version == 0
        && d.size == 0
        && d.reserved == 0
        && d.namespace == [0; 16]
        && d.data.is_null()
}
fn color(c: FrameColorMetadata) -> (u32, u32, u32, u32, u32, u32) {
    (
        c.version,
        c.size,
        c.primaries,
        c.transfer,
        c.matrix,
        c.range,
    )
}
#[derive(Clone, Copy)]
pub(super) struct SelectedInput {
    caps: Capabilities,
}
pub(super) enum Storage {
    DmaBuf(DmaBufFrame),
    Cpu(CpuFrame),
}
pub(super) struct InputFrame<'a> {
    source: ProcessingFrame<'a>,
    pub storage: Storage,
}
impl SelectedInput {
    pub fn frame<'a>(&self, source: ProcessingFrame<'a>) -> Result<InputFrame<'a>, Status> {
        let d = source.descriptor();
        if d.resource.data.is_null()
            || d.resource.reserved != 0
            || d.resource.namespace != [0; 16]
            || !none(&d.acquire_sync)
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        let storage = match self.caps {
            Capabilities::DmaBuf(caps)
                if d.resource.kind == FRAME_RESOURCE_DMABUF
                    && d.resource.version == 1
                    && d.resource.size as usize >= std::mem::size_of::<DmaBufFrame>() =>
            {
                let frame = unsafe { d.resource.data.cast::<DmaBufFrame>().read_unaligned() };
                frame.validate()?;
                if frame.fourcc != caps.fourcc
                    || frame.color != caps.color
                    || frame.modifier != caps.modifier
                    || frame.plane_count != 1
                    || (color(d.color) != (0, 0, 0, 0, 0, 0)
                        && color(d.color)
                            != (
                                1,
                                std::mem::size_of::<FrameColorMetadata>() as u32,
                                FRAME_PRIMARIES_BT709,
                                FRAME_TRANSFER_SRGB,
                                FRAME_MATRIX_RGB,
                                FRAME_RANGE_FULL,
                            ))
                    || !(caps.min_width..=caps.max_width).contains(&frame.width)
                    || !(caps.min_height..=caps.max_height).contains(&frame.height)
                {
                    return Err(MEDIA_UNSUPPORTED);
                }
                Storage::DmaBuf(frame)
            }
            Capabilities::Cpu(caps)
                if d.resource.kind == FRAME_RESOURCE_CPU
                    && d.resource.version == 2
                    && d.resource.size as usize >= std::mem::size_of::<CpuFrame>() =>
            {
                let frame = unsafe { d.resource.data.cast::<CpuFrame>().read_unaligned() };
                if frame.format != caps.base.format
                    || color(d.color) != color(caps.color)
                    || !(caps.base.min_width..=caps.base.max_width).contains(&frame.width)
                    || !(caps.base.min_height..=caps.base.max_height).contains(&frame.height)
                {
                    return Err(MEDIA_UNSUPPORTED);
                }
                // SAFETY: native capture lends readable immutable CPU pixels;
                // this validates length/stride without copying any pixel bytes.
                unsafe { frame.bytes()? };
                Storage::Cpu(frame)
            }
            _ => return Err(MEDIA_UNSUPPORTED),
        };
        let (width, height, timestamp, sequence) = match storage {
            Storage::DmaBuf(f) => (f.width, f.height, f.timestamp_us, f.sequence),
            Storage::Cpu(f) => (f.width, f.height, f.timestamp_us, f.sequence),
        };
        if (width, height, timestamp, sequence) != (d.width, d.height, d.timestamp_us, d.sequence) {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(InputFrame { source, storage })
    }
}
impl InputFrame<'_> {
    pub fn descriptor(&self) -> &CaptureResourceFrame {
        self.source.descriptor()
    }
    pub fn freshness(&self) -> u32 {
        self.descriptor().freshness
    }
    pub fn retain(&self) -> Result<RetainedProcessingInput, Status> {
        self.source.retain()
    }
}
