mod writer;

use std::sync::Arc;

use async_trait::async_trait;
use datafusion::catalog::{Session, TableProvider};
use datafusion::physical_plan::ExecutionPlan;
use datafusion_common::{not_impl_err, Result};
use sail_common_datafusion::datasource::{SinkInfo, SourceInfo, TableFormat};

use crate::formats::noop::writer::NoopSinkExec;

/// No-operation write format that consumes input without persisting data.
///
/// This is useful for benchmarking, profiling, dry-runs, and materializing
/// caches without the I/O overhead of writing to storage.
#[derive(Debug)]
pub struct NoopTableFormat;

#[async_trait]
impl TableFormat for NoopTableFormat {
    fn name(&self) -> &str {
        "noop"
    }

    async fn create_provider(
        &self,
        _ctx: &dyn Session,
        _info: SourceInfo,
    ) -> Result<Arc<dyn TableProvider>> {
        not_impl_err!("noop table format does not support reading")
    }

    async fn create_writer(
        &self,
        _ctx: &dyn Session,
        info: SinkInfo,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(NoopSinkExec::new(info.input)))
    }
}
