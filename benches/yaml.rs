//! Microbench: YAML parse/serialize hot paths.
//! Lock parsing happens N times during `ls`, same-SHA scan, and acquire's capacity check —
//! worth keeping <10µs per call.
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

#[path = "../src/yaml.rs"]
mod yaml;

const SAMPLE_LOCK: &str = "\
started_at: 2026-05-05T03:34:56Z
full_sha: abc123def456abc123def456abc123def4567890
group: ios
";

const SAMPLE_CONFIG: &str = "\
schema_version: 1
source: /Users/foo/Develop/myapp
default_commit: main
max_slots: 16
groups: ios,android
submodule_mirror_mode: bare-mirror
submodule_mirror_base: /Users/foo/Develop/mirrors
";

fn parse(c: &mut Criterion) {
    c.bench_function("yaml::parse lock", |b| {
        b.iter(|| yaml::parse(black_box(SAMPLE_LOCK)));
    });
    c.bench_function("yaml::parse config", |b| {
        b.iter(|| yaml::parse(black_box(SAMPLE_CONFIG)));
    });
}

fn serialize(c: &mut Criterion) {
    let pairs = vec![
        ("started_at", "2026-05-05T03:34:56Z".to_string()),
        ("full_sha", "abc123def456abc123def456abc123def4567890".to_string()),
        ("group", "ios".to_string()),
    ];
    c.bench_function("yaml::serialize lock", |b| {
        b.iter(|| yaml::serialize(black_box(&pairs)));
    });
}

criterion_group!(benches, parse, serialize);
criterion_main!(benches);
