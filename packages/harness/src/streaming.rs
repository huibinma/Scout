//! 搜索结果流式产出抽象。

use crate::SupportedIntent;
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use scout_search_backend::{SearchError, SearchResult};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::Instant;

/// 单次搜索流中的事件。
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ResultEvent {
    /// 搜索已开始。
    Started {
        /// 产生结果的工具 id。
        tool_id: String,
        /// 当前搜索对应的 intent 变体。
        intent: SupportedIntent,
    },
    /// 一条归一化搜索结果。
    Result(SearchResult),
    /// 搜索进度。同步后端无法报告时可以跳过。
    Progress {
        /// 当前已经产出的部分结果数。
        partial_count: usize,
    },
    /// 搜索正常结束。
    Finished {
        /// 总结果数。
        total: usize,
        /// 从包装开始到完成包装的耗时，单位毫秒。
        elapsed_ms: u64,
    },
    /// 搜索失败。
    Errored {
        /// 对上层可展示 / 可记录的错误详情。
        detail: String,
    },
}

/// 搜索结果流的取消信号。
///
/// v0.1 同步实现无法抢占已经进入 backend 的阻塞调用，但消费者可以在事件之间
/// 触发取消；[`ResultStream`] 会在产出下一项前检查该信号。
#[derive(Debug, Clone, Default)]
pub struct StreamCancellation {
    cancelled: Arc<AtomicBool>,
}

impl StreamCancellation {
    /// 创建一个未取消的新信号。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求取消后续事件产出。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 当前是否已经被取消。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 异步搜索结果事件流。
#[derive(Debug)]
pub struct ResultStream {
    receiver: mpsc::UnboundedReceiver<ResultEvent>,
    cancellation: StreamCancellation,
}

impl ResultStream {
    /// 从事件队列创建结果流，内部用 channel 统一 async 消费路径。
    #[must_use]
    pub fn new(events: Vec<ResultEvent>, cancellation: StreamCancellation) -> Self {
        let (mut sender, receiver) = mpsc::unbounded();
        for event in events {
            if cancellation.is_cancelled() {
                break;
            }
            if sender.start_send(event).is_err() {
                break;
            }
        }
        Self {
            receiver,
            cancellation,
        }
    }

    /// 返回当前流使用的取消信号。
    #[must_use]
    pub fn cancellation(&self) -> StreamCancellation {
        self.cancellation.clone()
    }
}

impl Stream for ResultStream {
    type Item = ResultEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.cancellation.is_cancelled() {
            return Poll::Ready(None);
        }
        self.receiver.poll_next_unpin(context)
    }
}

/// 把同步完整结果适配为 [`ResultStream`] 的上游入口。
#[derive(Debug, Clone)]
pub struct StreamSink {
    tool_id: String,
    intent: SupportedIntent,
    cancellation: StreamCancellation,
}

impl StreamSink {
    /// 创建一个用于指定工具与 intent 的 sink。
    #[must_use]
    pub fn new(tool_id: impl Into<String>, intent: SupportedIntent) -> Self {
        Self::with_cancellation(tool_id, intent, StreamCancellation::new())
    }

    /// 使用外部取消信号创建 sink。
    #[must_use]
    pub fn with_cancellation(
        tool_id: impl Into<String>,
        intent: SupportedIntent,
        cancellation: StreamCancellation,
    ) -> Self {
        Self {
            tool_id: tool_id.into(),
            intent,
            cancellation,
        }
    }

    /// 将一次性返回的完整结果包装为事件流。
    #[must_use]
    pub fn from_results(self, results: Vec<SearchResult>) -> ResultStream {
        let started_at = Instant::now();
        let total = results.len();
        let mut events = Vec::with_capacity(total.saturating_add(2));
        events.push(ResultEvent::Started {
            tool_id: self.tool_id,
            intent: self.intent,
        });
        events.extend(results.into_iter().map(ResultEvent::Result));
        events.push(ResultEvent::Finished {
            total,
            elapsed_ms: elapsed_millis(started_at),
        });
        ResultStream::new(events, self.cancellation)
    }

    /// 将错误包装为 `Started -> Errored` 事件流。
    #[must_use]
    pub fn from_error(self, detail: impl Into<String>) -> ResultStream {
        ResultStream::new(
            vec![
                ResultEvent::Started {
                    tool_id: self.tool_id,
                    intent: self.intent,
                },
                ResultEvent::Errored {
                    detail: detail.into(),
                },
            ],
            self.cancellation,
        )
    }
}

/// 为 MVP-07A 预留的同步转流接口。
pub trait IntoStream {
    /// 消费自身并返回 [`ResultStream`]。
    fn into_stream(self, sink: StreamSink) -> ResultStream;
}

impl IntoStream for Vec<SearchResult> {
    fn into_stream(self, sink: StreamSink) -> ResultStream {
        sink.from_results(self)
    }
}

impl IntoStream for Result<Vec<SearchResult>, SearchError> {
    fn into_stream(self, sink: StreamSink) -> ResultStream {
        match self {
            Ok(results) => sink.from_results(results),
            Err(error) => sink.from_error(error.to_string()),
        }
    }
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use futures_util::StreamExt;
    use scout_search_backend::{BackendKind, MatchType, SearchResultMetadata};
    use std::path::PathBuf;

    fn result(id: &str) -> SearchResult {
        SearchResult {
            id: id.to_owned(),
            path: PathBuf::from(format!("/tmp/{id}.txt")),
            name: format!("{id}.txt"),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    use futures_executor::block_on;

    #[test]
    fn empty_backend_emits_started_and_finished() {
        let events: Vec<_> = block_on(
            StreamSink::new("search.spotlight", SupportedIntent::FileSearch)
                .from_results(Vec::new())
                .collect(),
        );

        assert_eq!(
            events[0],
            ResultEvent::Started {
                tool_id: "search.spotlight".to_owned(),
                intent: SupportedIntent::FileSearch,
            }
        );
        assert!(matches!(
            events[1],
            ResultEvent::Finished {
                total: 0,
                elapsed_ms: _
            }
        ));
    }

    #[test]
    fn multiple_results_emit_before_finished() {
        let events: Vec<_> = block_on(
            StreamSink::new("search.spotlight", SupportedIntent::FileSearch)
                .from_results(vec![result("a"), result("b")])
                .collect(),
        );

        assert_eq!(events.len(), 4);
        assert!(matches!(events[1], ResultEvent::Result(_)));
        assert!(matches!(events[2], ResultEvent::Result(_)));
        assert!(matches!(
            events[3],
            ResultEvent::Finished {
                total: 2,
                elapsed_ms: _
            }
        ));
    }

    #[test]
    fn cancellation_stops_before_next_event() {
        let cancellation = StreamCancellation::new();
        let mut stream = StreamSink::with_cancellation(
            "search.spotlight",
            SupportedIntent::FileSearch,
            cancellation.clone(),
        )
        .from_results(vec![result("a"), result("b")]);

        assert!(matches!(
            block_on(stream.next()),
            Some(ResultEvent::Started { .. })
        ));
        cancellation.cancel();
        assert_eq!(block_on(stream.next()), None);
    }

    #[test]
    fn error_emits_started_and_errored() {
        let stream = Err::<Vec<SearchResult>, SearchError>(SearchError::BackendUnavailable {
            reason: "missing index".to_owned(),
        })
        .into_stream(StreamSink::new(
            "search.spotlight",
            SupportedIntent::FileSearch,
        ));
        let events: Vec<_> = block_on(stream.collect());

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ResultEvent::Started { .. }));
        assert_eq!(
            events[1],
            ResultEvent::Errored {
                detail: "backend unavailable: missing index".to_owned(),
            }
        );
    }
}
