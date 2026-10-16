//! Wire declarations from the official 4.40.1 descriptor.
use prost::{Message, Oneof};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Windows 纪元（1601-01-01）与 Unix 纪元之间的秒数差。
pub(super) const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;
/// 一秒有多少个 100ns tick。
pub(super) const TICKS_PER_SECOND: u64 = 10_000_000;
/// 线上时间戳小于它只可能是 Unix 秒：1970 年之后的 ticks 至少
/// `FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND`（1.16e17），而 1e12 秒已经是公元 33658 年。
const SECONDS_CEILING: u64 = 1_000_000_000_000;
/// 这一层能表示的最大 Unix 秒（9999-12-31），再大就是回绕或坏值。
const MAX_UNIX_SECONDS: u64 = 253_402_300_799;

/// 线上 `last_write_time` 统一成 100ns ticks（1601 纪元）。
///
/// 这个字段官方和我们自己发的量纲不一样：官方 Windows 客户端发 Unix 秒，我们自己的
/// 客户端把本地剪贴板的 `FILEDESCRIPTORW` 原样透传、发 ticks。实机同一份文件分两次
/// 复制分别量到 `1791981679` 与 `134364552795785604`——前者按 Unix 秒读正是后者的
/// 整秒部分。两套量纲差五个数量级、不重叠，所以不用猜。`0`（未知）保持 `0`。
/// 只对**线上来的**值用它：自己从文件元数据取的时刻本来就是 ticks。
pub(super) fn wire_filetime(value: u64) -> u64 {
    if value == 0 || value >= SECONDS_CEILING {
        value
    } else {
        value * TICKS_PER_SECOND + FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND
    }
}

/// 反过来：[`wire_filetime`] 的逆，把进程内部的 ticks 写回线上，线上统一发 Unix 秒。
///
/// 依据是官方客户端自己的做法（实机量到它发 `1791981679`），而我们的 Windows 客户端
/// 以前是把自己那边 `FILEDESCRIPTORW` 的 FILETIME 原样透传，于是官方粘贴我们复制过去的
/// 文件时「修改日期」是空的——它按秒读，我们发的是 ticks。读的那一侧两种都认
/// （`wire_filetime`），所以线上改成秒不会破坏我们自己的两个客户端。
pub(super) fn wire_seconds(ticks: u64) -> u64 {
    // 小于 1601 纪元（只可能是 Unix 秒或 0）的原样通过，免得来回转换把值搅坏。
    if ticks < FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND {
        return ticks;
    }
    (ticks - FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND) / TICKS_PER_SECOND
}

/// 100ns ticks 读成本地时间。未知（`0`）、回绕和超出 9999 年的值都落到 Unix 纪元，
/// 也就是 1970——这一层没有第二个「没有时间」的表达。
pub(super) fn local_mtime(ticks: u64) -> SystemTime {
    let seconds = ticks / TICKS_PER_SECOND;
    if !(FILETIME_EPOCH_OFFSET..=FILETIME_EPOCH_OFFSET + MAX_UNIX_SECONDS).contains(&seconds) {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH
        + Duration::new(seconds - FILETIME_EPOCH_OFFSET, (ticks % TICKS_PER_SECOND * 100) as u32)
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

#[cfg(test)]
mod time_tests {
    use super::*;

    /// 实机从官方 Windows 客户端量到的值。
    const OFFICIAL_SECONDS: u64 = 1_791_981_679;
    /// 实机从我们自己 Windows 客户端量到的、同一时刻的另一个量纲。
    const OURS_TICKS: u64 = 134_364_552_795_785_604;

    #[test]
    fn either_scale_reads_as_the_same_instant() {
        assert_eq!(
            wire_filetime(OFFICIAL_SECONDS),
            OFFICIAL_SECONDS * TICKS_PER_SECOND + FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND
        );
        // 本来就是 ticks 的值原样通过。
        assert_eq!(wire_filetime(OURS_TICKS), OURS_TICKS);
        assert_eq!(
            local_mtime(wire_filetime(OFFICIAL_SECONDS))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            OFFICIAL_SECONDS
        );
        // 我们发的那一份带 100ns 小数，读回来要保住。
        assert_eq!(
            local_mtime(OURS_TICKS).duration_since(UNIX_EPOCH).unwrap().as_secs(),
            OFFICIAL_SECONDS
        );
        assert_eq!(
            local_mtime(OURS_TICKS).duration_since(UNIX_EPOCH).unwrap().subsec_nanos(),
            578_560_400
        );
    }

    #[test]
    fn sends_seconds_reads_ticks() {
        // 我们内部是 ticks，线上发秒；读回来还是同一秒（100ns 的小数部分会丢，
        // 官方那条路本来也只有秒的精度）。
        assert_eq!(
            wire_filetime(wire_seconds(OURS_TICKS)),
            OFFICIAL_SECONDS * TICKS_PER_SECOND + FILETIME_EPOCH_OFFSET * TICKS_PER_SECOND
        );
        // 已经是秒的值不会被二次换算。
        assert_eq!(wire_seconds(OFFICIAL_SECONDS), OFFICIAL_SECONDS);
        assert_eq!(wire_seconds(0), 0);
        assert_eq!(wire_filetime(wire_seconds(0)), 0);
    }

    #[test]
    fn unknown_and_wrapped_times_have_no_stamp() {
        assert_eq!(wire_filetime(0), 0);
        assert_eq!(local_mtime(0), UNIX_EPOCH);
        assert_eq!(local_mtime(u64::MAX), UNIX_EPOCH);
    }
}
