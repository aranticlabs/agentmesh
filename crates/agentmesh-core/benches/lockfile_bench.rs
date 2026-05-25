use std::collections::BTreeMap;
use std::path::PathBuf;

use agentmesh_core::{
    EntityId, EntityType, Hash, LocationKey, RuntimeName,
    lockfile::{
        AdapterDeclaration, AdapterMode, HookKind, Lockfile, LockfileEntity, parse_lockfile,
        serialize_lockfile,
    },
};
use criterion::{Criterion, criterion_group, criterion_main};

fn sample_lockfile(entity_count: usize) -> Lockfile {
    let mut lockfile = Lockfile::empty();
    lockfile.adapters.insert(
        RuntimeName::new("claude").expect("runtime name should be valid"),
        AdapterDeclaration {
            mode: AdapterMode::Bundled,
            protocol_version: 1,
            entities: vec![
                EntityType::Instructions,
                EntityType::Skill,
                EntityType::Subagent,
            ],
            hooks: vec![HookKind::PostToolUse],
        },
    );
    lockfile.adapters.insert(
        RuntimeName::new("codex").expect("runtime name should be valid"),
        AdapterDeclaration {
            mode: AdapterMode::Bundled,
            protocol_version: 1,
            entities: vec![
                EntityType::Instructions,
                EntityType::Skill,
                EntityType::Subagent,
            ],
            hooks: vec![HookKind::PostToolUse],
        },
    );

    let canonical = LocationKey::new(".ai").expect("location key should be valid");
    let claude = LocationKey::new(".claude").expect("location key should be valid");
    let codex = LocationKey::new(".codex").expect("location key should be valid");
    for index in 0..entity_count {
        let slug = format!("skill-{index}");
        let id = EntityId::from_parts(EntityType::Skill, &slug).expect("entity id should be valid");
        let hash = Hash::new(format!("{:064x}", index + 1)).expect("hash should be valid");
        lockfile.entities.insert(
            id,
            LockfileEntity {
                entity_type: EntityType::Skill,
                scope: None,
                locations: BTreeMap::from([
                    (
                        canonical.clone(),
                        PathBuf::from(format!("skills/{slug}/SKILL.md")),
                    ),
                    (
                        claude.clone(),
                        PathBuf::from(format!("skills/{slug}/SKILL.md")),
                    ),
                    (
                        codex.clone(),
                        PathBuf::from(format!("skills/{slug}/SKILL.md")),
                    ),
                ]),
                canonical_sha256: hash.clone(),
                emitted_native_sha256: BTreeMap::from([
                    (claude.clone(), hash.clone()),
                    (codex.clone(), hash),
                ]),
                lineage: Vec::new(),
                pending_conflict_resolution: None,
                rename_history: Vec::new(),
                id_pin: None,
            },
        );
    }
    lockfile
}

fn lockfile_parse_serialize_benchmark(c: &mut Criterion) {
    let lockfile = sample_lockfile(1000);
    let serialized = serialize_lockfile(&lockfile).expect("lockfile should serialize");

    c.bench_function("lockfile_parse_1000_entities", |b| {
        b.iter(|| parse_lockfile(&serialized))
    });
    c.bench_function("lockfile_serialize_1000_entities", |b| {
        b.iter(|| serialize_lockfile(&lockfile))
    });
}

criterion_group!(benches, lockfile_parse_serialize_benchmark);
criterion_main!(benches);
