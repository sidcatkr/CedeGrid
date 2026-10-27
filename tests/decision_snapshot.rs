//! Seeded decision sequences for the decision snapshot (tests/support/snapshot.rs):
//! CPU, RAM and GPU in all three modes, with unknown readings, external users
//! that arrive and leave, and own work that follows the steps it is given.
#[allow(dead_code)]
mod snapshot {
    include!("support/snapshot.rs");
}

use cedegrid::{
    config::Config,
    contention::{ContentionMode, ResourceKey, ResourceKind, YieldStep},
    execution_model::AllocationClass,
    kernel::{CpuPressure, CpuPressureStatus, KernelCollector, KernelConfig},
    model::*,
};
use snapshot::PolicyEngine;
use std::collections::{BTreeMap, BTreeSet};

const SEQUENCES: u64 = 500;
const INTERVALS: u64 = 50;
const INTERVAL_MS: u64 = 500;
const CPU: u64 = 8000;
const RAM: u64 = 32_000;
const VRAM: u64 = 16_000;
const GPU: &str = "GPU-test";

/// SplitMix64, so a seed gives the same sequence on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// A step of up to 15% either way, kept within bounds.
    fn walk(&mut self, value: u64, high: u64) -> u64 {
        let step = value * 15 / 100 + 1;
        (value + self.below(step + 1))
            .saturating_sub(self.below(step + 1))
            .min(high)
    }
}

struct Job {
    id: String,
    class: AllocationClass,
    requested: Resources,
    /// What it would use with no step applied.
    wants: (u64, u64, u64),
    modes: BTreeMap<ResourceKind, ContentionMode>,
    facts: AllocationFacts,
    /// Steps and targets from the last decision, as the supervisor applied them.
    applied: BTreeMap<ResourceKey, (YieldStep, Option<u64>)>,
    gone: bool,
}

struct External {
    uid: u32,
    arrives: u64,
    leaves: u64,
    cpu: u64,
    ram: u64,
    vram: u64,
    compute: u64,
}

fn config(rng: &mut Rng) -> Config {
    let mut config = Config::default();
    config.contention.default_mode = ContentionMode::ALL[rng.below(3) as usize];
    config.cpu.reserve_physical_cores = u32::try_from(rng.below(2)).unwrap();
    config.ram.reserve_mib = 1000;
    config.ram.reserve_percent = 0;
    config.gpu.reserve_vram_mib = 1000;
    config.gpu.reserve_vram_percent = 0;
    config.gpu.scale_up_cooldown_ms = rng.chance(50).then_some(0);
    if rng.chance(50) {
        config.contention.cpu.scale_up_cooldown_ms = Some(0);
    }
    for caution in ["small_floor", "performance_mode"] {
        config.contention.acknowledge_caution.insert(caution.into());
    }
    config
}

fn job(rng: &mut Rng, index: u64) -> Job {
    let (cpu, ram) = (rng.between(500, 4000), rng.between(512, 8000));
    let vram = if rng.chance(50) {
        rng.between(1000, 6000)
    } else {
        0
    };
    let mut modes = BTreeMap::new();
    if rng.chance(20) {
        let kind = ResourceKind::ALL[rng.below(4) as usize];
        modes.insert(kind, ContentionMode::ALL[rng.below(3) as usize]);
    }
    let mut unavailable = BTreeMap::new();
    if rng.chance(70) {
        unavailable.insert(ResourceKey::Cpu, BTreeSet::from([YieldStep::LowerShare]));
    }
    Job {
        id: format!("job-{index}"),
        class: if rng.chance(20) {
            AllocationClass::Guaranteed
        } else {
            AllocationClass::Opportunistic
        },
        requested: resources(cpu, ram, vram),
        wants: (
            rng.between(cpu / 2, CPU),
            rng.between(ram / 2, ram * 2),
            vram,
        ),
        modes,
        facts: AllocationFacts {
            unavailable,
            hint_ready: rng.chance(60),
            observes_drain: rng.chance(60),
            ram_cap: rng.chance(30),
            ..AllocationFacts::default()
        },
        applied: BTreeMap::new(),
        gone: false,
    }
}

fn external(rng: &mut Rng, index: u32) -> External {
    let arrives = rng.below(INTERVALS);
    External {
        uid: 2000 + index,
        arrives,
        leaves: arrives + rng.between(5, INTERVALS),
        cpu: rng.between(100, CPU),
        ram: rng.between(256, 12_000),
        vram: if rng.chance(40) {
            rng.between(500, 8000)
        } else {
            0
        },
        compute: if rng.chance(40) {
            rng.between(50, 1000)
        } else {
            0
        },
    }
}

fn resources(cpu: u64, ram: u64, vram: u64) -> Resources {
    Resources {
        cpu_millicores: cpu,
        ram_mib: ram,
        gpu_memory_mib: if vram == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(GPU.to_string(), vram)])
        },
    }
}

/// What a job uses under the step it was given.
fn usage(job: &Job) -> (u64, u64, u64) {
    let limited = |key: &ResourceKey, want: u64| match job.applied.get(key) {
        Some((YieldStep::Pause, _)) if *key == ResourceKey::Cpu => 0,
        Some((YieldStep::Cap | YieldStep::Hint, Some(target))) => want.min(*target),
        _ => want,
    };
    (
        limited(&ResourceKey::Cpu, job.wants.0),
        limited(&ResourceKey::Ram, job.wants.1),
        limited(&ResourceKey::GpuMemory(GPU.into()), job.wants.2),
    )
}

fn pressure(rng: &mut Rng) -> Option<CpuPressure> {
    let status = match rng.below(4) {
        0 => return None,
        1 => CpuPressureStatus::SystemOnly,
        2 => CpuPressureStatus::Unknown,
        _ => CpuPressureStatus::Available,
    };
    let external = (status == CpuPressureStatus::Available).then(|| rng.below(60) as f64);
    Some(CpuPressure {
        status,
        external_some_avg10: external,
        external_cgroups: u32::from(external.is_some()) * 3,
        own_some_avg10: Some(rng.below(60) as f64),
        system_some_avg10: Some(rng.below(60) as f64),
        detail: String::new(),
    })
}

/// One seeded sequence; returns how many intervals the policy refused.
fn sequence(seed: u64) -> usize {
    let mut rng = Rng(seed);
    let config = config(&mut rng);
    let mut jobs: Vec<Job> = (0..rng.between(1, 3)).map(|i| job(&mut rng, i)).collect();
    let externals: Vec<External> = (0..rng.between(0, 3))
        .map(|i| external(&mut rng, u32::try_from(i).unwrap()))
        .collect();
    let measured_compute = rng.chance(50);
    let with_kernel = rng.chance(40);
    let mut engine = PolicyEngine::new();
    let mut refused = 0;
    let mut kernel = KernelCollector::new(KernelConfig {
        enabled: false,
        ..KernelConfig::default()
    })
    .sample();
    kernel.runtime_os = "fixture".into();
    for interval in 0..INTERVALS {
        for job in jobs.iter_mut().filter(|job| !job.gone) {
            job.wants = (
                rng.walk(job.wants.0, CPU),
                rng.walk(job.wants.1, RAM / 2),
                job.wants.2,
            );
        }
        let present: Vec<(u32, u64, u64, u64, u64)> = externals
            .iter()
            .filter(|e| e.arrives <= interval && interval < e.leaves)
            .map(|e| {
                let cpu = rng.walk(e.cpu, CPU);
                (e.uid, cpu, e.ram, e.vram, e.compute)
            })
            .collect();
        let running: Vec<&Job> = jobs.iter().filter(|job| !job.gone).collect();
        let own: Vec<(u64, u64, u64)> = running.iter().map(|job| usage(job)).collect();
        let sum = |pick: fn(&(u64, u64, u64)) -> u64| own.iter().map(pick).sum::<u64>();
        let (own_cpu, own_ram, own_vram) = (sum(|u| u.0), sum(|u| u.1), sum(|u| u.2));
        let ext_cpu: u64 = present.iter().map(|e| e.1).sum();
        let ext_ram: u64 = present.iter().map(|e| e.2).sum();
        let ext_vram: u64 = present.iter().map(|e| e.3).sum();
        let ext_compute: u64 = present.iter().map(|e| e.4).sum();
        let busy = (own_cpu + ext_cpu + rng.below(200)).min(CPU);
        let mut snap = Snapshot {
            schema_version: SCHEMA_VERSION,
            node_id: "local".into(),
            observed_at_unix_ms: 100 + interval * INTERVAL_MS,
            cpu_capacity_millicores: CPU,
            physical_cores: Some(4),
            cpu_busy_millicores: (!rng.chance(5)).then_some(busy),
            cpu_busy_lower_millicores: None,
            managed_cpu_lower_millicores: None,
            managed_cpu_window_lower_millicores: None,
            cedegrid_cpu_lower_millicores: None,
            cedegrid_cpu_window_lower_millicores: None,
            total_ram_mib: RAM,
            available_ram_mib: (!rng.chance(5))
                .then(|| RAM.saturating_sub(own_ram + ext_ram + 2000)),
            gpu_inventory: CapabilityStatus::Available,
            gpus: vec![GpuSnapshot {
                uuid: GPU.into(),
                total_memory_mib: VRAM,
                used_memory_mib: (!rng.chance(5)).then(|| (own_vram + ext_vram + 300).min(VRAM)),
                utilization_percent: Some(u32::try_from(rng.below(101)).unwrap()),
                external_process_ids: (!rng.chance(5)).then(|| {
                    present
                        .iter()
                        .filter(|e| e.3 > 0 || e.4 > 0)
                        .map(|e| e.0 * 10)
                        .collect()
                }),
                external_compute: if rng.chance(5) {
                    ComputeActivity::Unknown
                } else if ext_compute > 0 {
                    ComputeActivity::Active
                } else {
                    ComputeActivity::Idle
                },
                compute_sample_id: Some(interval + 1),
                observation: None,
                reserved_memory_mib: None,
                free_memory_mib: None,
                unattributed_memory_mib: None,
                external_compute_milli: measured_compute.then_some(ext_compute.min(1000)),
                own_compute_milli: measured_compute.then_some(0),
                compute_amount_unknown: None,
                own_only: false,
            }],
            capabilities: BTreeMap::new(),
            kernel: None,
            principals: None,
            external_processes: None,
        };
        if !rng.chance(5) {
            let mut principals = vec![PrincipalUsage {
                uid: Some(1000),
                is_owner: true,
                cpu_millicores: rng.below(200),
                ..PrincipalUsage::default()
            }];
            principals.extend(present.iter().map(|e| {
                PrincipalUsage {
                    uid: Some(e.0),
                    is_owner: false,
                    cpu_millicores: e.1,
                    ram_mib: e.2,
                    gpu_memory_mib: [(GPU.to_string(), e.3)]
                        .into_iter()
                        .filter(|(_, v)| *v > 0)
                        .collect(),
                    gpu_compute_milli: [(GPU.to_string(), e.4)]
                        .into_iter()
                        .filter(|(_, v)| measured_compute && *v > 0)
                        .collect(),
                }
            }));
            snap.principals = Some(principals);
        }
        if !rng.chance(5) {
            snap.external_processes = Some(
                present
                    .iter()
                    .map(|e| ExternalProcessMemory {
                        pid: e.0 * 10,
                        start_time: 1000 + u64::from(e.0),
                        ram_mib: e.2,
                        gpu_memory_mib: [(GPU.to_string(), e.3)]
                            .into_iter()
                            .filter(|(_, v)| *v > 0)
                            .collect(),
                    })
                    .collect(),
            );
        }
        if with_kernel {
            kernel.cpu_pressure = pressure(&mut rng);
            snap.kernel = Some(kernel.clone());
        }
        let input = PolicyInput {
            allocations: running
                .iter()
                .zip(&own)
                .map(|(job, used)| Allocation {
                    id: job.id.clone(),
                    class: job.class,
                    phase: AllocationPhase::Running,
                    requested: job.requested.clone(),
                    observed: (!rng.chance(3)).then(|| resources(used.0, used.1, used.2)),
                    observed_upper: None,
                    observed_window_cpu: None,
                    modes: job.modes.clone(),
                })
                .collect(),
            facts: running
                .iter()
                .map(|job| {
                    let mut facts = job.facts.clone();
                    for (key, (step, target)) in &job.applied {
                        facts.applied.insert(key.clone(), *step);
                        if let Some(target) = target {
                            facts.applied_targets.insert(key.clone(), *target);
                        }
                    }
                    (job.id.clone(), facts)
                })
                .collect(),
            ..PolicyInput::default()
        };
        let Ok(decision) = engine.evaluate(&config, &snap, &input, interval * INTERVAL_MS) else {
            refused += 1;
            continue;
        };
        for job in jobs.iter_mut().filter(|job| !job.gone) {
            job.applied = decision
                .actions
                .iter()
                .filter(|a| a.allocation_id == job.id)
                .map(|a| (a.resource.clone(), (a.step, a.target)))
                .collect();
            job.gone = job
                .applied
                .values()
                .any(|(step, _)| *step >= YieldStep::Drain);
        }
    }
    refused
}

/// The generated input is input the policy accepts, so the snapshot holds
/// decisions rather than refusals.
#[test]
fn generated_sequences_are_accepted() {
    for seed in 0..20 {
        assert_eq!(sequence(seed), 0, "seed {seed}");
    }
}

#[test]
#[ignore = "decision snapshot: needs CEDEGRID_DECISION_SNAPSHOT"]
fn decision_snapshot() {
    snapshot::check(
        "random",
        (0..SEQUENCES)
            .map(|seed| {
                let run = move || {
                    sequence(seed);
                };
                (format!("random::{seed}"), Box::new(run) as Box<dyn Fn()>)
            })
            .collect(),
    );
}
