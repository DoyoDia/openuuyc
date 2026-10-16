//! Wire declarations from the official 4.40.1 descriptor.
use prost::{Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFormat {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DragDropAutoSave {
    #[prost(string, tag = "1")]
    pub dest_path: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DragDropAutoSaveComplete {
    #[prost(uint32, tag = "1")]
    pub total_count: u32,
    #[prost(uint32, tag = "2")]
    pub success_count: u32,
    #[prost(string, tag = "3")]
    pub first_file_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DragDropAutoSaveCompleteResponse {
    #[prost(int32, tag = "1")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct DragDropOleDrop {
    #[prost(double, tag = "1")]
    pub target_x: f64,
    #[prost(double, tag = "2")]
    pub target_y: f64,
    #[prost(int32, tag = "3")]
    pub screen_id: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFormatListRequest {
    #[prost(message, repeated, tag = "1")]
    pub formats: Vec<ClipboardFormat>,
    #[prost(int32, tag = "2")]
    pub has_action: i32,
    #[prost(oneof = "ClipboardFormatListRequestKind", tags = "3, 4")]
    pub drag_drop_action: Option<ClipboardFormatListRequestKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClipboardFormatListRequestKind {
    #[prost(message, tag = "3")]
    AutoSave(DragDropAutoSave),
    #[prost(message, tag = "4")]
    OleDrop(DragDropOleDrop),
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFormatListResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFormatDataAsk {
    #[prost(uint32, tag = "1")]
    pub format_id: u32,
    #[prost(string, tag = "2")]
    pub block_key: String,
    #[prost(string, tag = "3")]
    pub format_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFormatDataConfirm {
    #[prost(int32, tag = "1")]
    pub err: i32,
    #[prost(string, tag = "2")]
    pub block_key: String,
    #[prost(int32, tag = "3")]
    pub block_count: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardDataBlock {
    #[prost(string, tag = "1")]
    pub block_key: String,
    #[prost(int32, tag = "2")]
    pub block_id: i32,
    #[prost(bytes, tag = "3")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardDataBlockConfirm {
    #[prost(string, tag = "1")]
    pub block_key: String,
    #[prost(int32, tag = "2")]
    pub block_id: i32,
    #[prost(int32, tag = "3")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileDescriptor {
    #[prost(string, tag = "1")]
    pub file_name: String,
    #[prost(uint32, tag = "2")]
    pub file_attributes: u32,
    #[prost(uint64, tag = "3")]
    pub last_write_time: u64,
    #[prost(uint64, tag = "4")]
    pub file_size: u64,
}

// UU 4.41.2.2602 C:F2D630/F32A50 sends Unix seconds; F3A7C0
// converts received seconds to FILETIME for the Windows OLE descriptor.
const FILETIME_EPOCH_SECONDS: u64 = 11_644_473_600;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Convert wire Unix seconds to native FILETIME. Zero/invalid stays unspecified.
pub(super) fn wire_filetime(seconds: u64) -> u64 {
    if seconds == 0 {
        return 0;
    }
    seconds
        .checked_add(FILETIME_EPOCH_SECONDS)
        .and_then(|v| v.checked_mul(TICKS_PER_SECOND))
        .filter(|&ticks| ticks <= i64::MAX as u64)
        .unwrap_or(0)
}

/// Convert native FILETIME to the wire's whole Unix seconds, without guessing units.
pub(super) fn wire_seconds(ticks: u64) -> u64 {
    if ticks > i64::MAX as u64 {
        return 0;
    }
    ticks
        .checked_sub(FILETIME_EPOCH_SECONDS * TICKS_PER_SECOND)
        .map_or(0, |elapsed| elapsed / TICKS_PER_SECOND)
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileDescriptorListRequest {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileDescriptorListResponse {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(uint32, tag = "2")]
    pub segment_count: u32,
    #[prost(int32, tag = "3")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileDescriptorSegment {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(uint32, tag = "2")]
    pub segment_id: u32,
    #[prost(message, repeated, tag = "3")]
    pub file_descs: Vec<ClipboardFileDescriptor>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileDescriptorSegmentConfirm {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(uint32, tag = "2")]
    pub segment_id: u32,
    #[prost(int32, tag = "3")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileContentsRequest {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(uint32, tag = "2")]
    pub list_index: u32,
    #[prost(uint32, tag = "3")]
    pub flags: u32,
    #[prost(uint64, tag = "4")]
    pub pos_offset: u64,
    #[prost(uint32, tag = "5")]
    pub requested_len: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileContentsResponse {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(bytes, tag = "2")]
    pub data: Vec<u8>,
    #[prost(int32, tag = "3")]
    pub err: i32,
    #[prost(uint64, tag = "4")]
    pub pos_offset: u64,
    #[prost(uint32, tag = "5")]
    pub list_index: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileCancelRequest {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardFileCancelResponse {
    #[prost(uint32, tag = "1")]
    pub task_id: u32,
    #[prost(int32, tag = "2")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardTextChangeRequest {
    #[prost(uint32, tag = "1")]
    pub format_id: u32,
    #[prost(string, tag = "2")]
    pub data: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardTextChangeResponse {
    #[prost(int32, tag = "1")]
    pub err: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardRequest {
    #[prost(oneof = "ClipboardRequestKind", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub which: Option<ClipboardRequestKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClipboardRequestKind {
    #[prost(message, tag = "1")]
    FormatList(ClipboardFormatListRequest),
    #[prost(message, tag = "2")]
    FormatDataAsk(ClipboardFormatDataAsk),
    #[prost(message, tag = "3")]
    DataBlock(ClipboardDataBlock),
    #[prost(message, tag = "4")]
    FileDescListRequest(ClipboardFileDescriptorListRequest),
    #[prost(message, tag = "5")]
    FileContentsRequest(ClipboardFileContentsRequest),
    #[prost(message, tag = "6")]
    CancelRequest(ClipboardFileCancelRequest),
    #[prost(message, tag = "7")]
    DescSegment(ClipboardFileDescriptorSegment),
    #[prost(message, tag = "8")]
    AutoSaveComplete(DragDropAutoSaveComplete),
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipboardResponse {
    #[prost(oneof = "ClipboardResponseKind", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub which: Option<ClipboardResponseKind>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClipboardResponseKind {
    #[prost(message, tag = "1")]
    FormatListResponse(ClipboardFormatListResponse),
    #[prost(message, tag = "2")]
    FormatDataConfirm(ClipboardFormatDataConfirm),
    #[prost(message, tag = "3")]
    DataBlockConfirm(ClipboardDataBlockConfirm),
    #[prost(message, tag = "4")]
    FileDescListResponse(ClipboardFileDescriptorListResponse),
    #[prost(message, tag = "5")]
    FileContentsResponse(ClipboardFileContentsResponse),
    #[prost(message, tag = "6")]
    CancelResponse(ClipboardFileCancelResponse),
    #[prost(message, tag = "7")]
    DescSegmentConfirm(ClipboardFileDescriptorSegmentConfirm),
    #[prost(message, tag = "8")]
    AutoSaveCompleteResponse(DragDropAutoSaveCompleteResponse),
}
