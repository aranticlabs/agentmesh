use std::time::{Duration, SystemTime};

use agentmesh_core::merge::merge_markdown;
use criterion::{Criterion, criterion_group, criterion_main};

fn merge_markdown_benchmark(c: &mut Criterion) {
    let ancestor = "---\nname: demo\ntags:\n  - rust\n---\nLine one\nLine two\n";
    let current = "---\nname: demo\ntags:\n  - rust\n  - cli\n---\nLine one\nLine two\n";
    let incoming = "---\nname: demo\ntags:\n  - rust\n---\nLine one\nLine two\nLine three\n";
    let current_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let incoming_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(3);

    c.bench_function("merge_markdown_non_overlapping", |b| {
        b.iter(|| merge_markdown(ancestor, current, incoming, current_mtime, incoming_mtime))
    });
}

criterion_group!(benches, merge_markdown_benchmark);
criterion_main!(benches);
