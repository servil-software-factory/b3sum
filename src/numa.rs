//! Linux NUMA node confinement for the CLI process.
//!
//! Keeps `b3sum`'s main thread on one memory node before it starts its file
//! readers and hash workers, where `/sys/devices/system` exposes more than
//! one online node and `sched_setaffinity` allows the binding. Mirrors
//! `blake3-servil`'s worker-pool node selection without requiring any
//! undocumented hook in `blake3-servil`.

use std::path::Path;

const CPUS: usize = 1024;
type Set = [u64; CPUS / 64];

fn has(set: &Set, cpu: usize) -> bool {
    cpu < CPUS && (set[cpu / 64] & 1u64.rotate_left(u32::try_from(cpu).expect("cpu fits u32"))) != 0
}

fn number(text: &str) -> Option<usize> {
    let n: usize = text.trim().parse().ok()?;
    (n < CPUS).then_some(n)
}

fn cpu_set(text: &str) -> Option<Set> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut set = [0u64; CPUS / 64];
    for part in trimmed.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (number(a)?, number(b)?),
            None => {
                let n = number(part)?;
                (n, n)
            }
        };
        if first > last {
            return None;
        }
        for cpu in first..=last {
            set[cpu / 64] |= 1u64.rotate_left(u32::try_from(cpu).expect("cpu fits u32"));
        }
    }
    Some(set)
}

fn online_nodes(online: std::io::Result<String>) -> Option<Set> {
    cpu_set(&online.ok()?).filter(|nodes| nodes.iter().map(|word| word.count_ones()).sum::<u32>() > 1)
}

fn affinity() -> Option<Set> {
    let mut set = [0u64; CPUS / 64];
    // Sound: `set` is `size_of::<Set>()` writable bytes aligned to `u64` (at least `cpu_set_t` alignment).
    let rc = unsafe { libc::sched_getaffinity(0, core::mem::size_of::<Set>(), set.as_mut_ptr().cast()) };
    (rc == 0).then_some(set)
}

fn named(system: &Path, online: &Set, cpu: usize) -> Option<usize> {
    let mut nodes =
        std::fs::read_dir(system.join(format!("cpu/cpu{cpu}"))).ok()?.filter_map(|entry| number(entry.ok()?.file_name().to_str()?.strip_prefix("node")?));
    let node = nodes.next()?;
    (nodes.next().is_none() && has(online, node)).then_some(node)
}

fn listed(system: &Path, node: usize) -> Option<Set> {
    cpu_set(&std::fs::read_to_string(system.join(format!("node/node{node}/cpulist"))).ok()?)
}

fn cpu_count(set: &Set) -> usize {
    set.iter().map(|word| word.count_ones() as usize).sum()
}

fn within(allowed: Set, node: Set) -> Option<Set> {
    let mask: Set = core::array::from_fn(|i| allowed[i] & node[i]);
    if cpu_count(&mask) == 0 {
        return None;
    }
    (mask != allowed).then_some(mask)
}

fn largest(system: &Path, online: &Set, cpu: usize, allowed: Set) -> Option<Set> {
    let here = named(system, online, cpu)?;
    let own_list = listed(system, here)?;
    if !has(&own_list, cpu) {
        return None;
    }
    let own = within(allowed, own_list)?;
    let mut said: Vec<(usize, Set)> = Vec::new();
    for other in (0..CPUS).filter(|&other| has(&allowed, other)) {
        let Some(node) = named(system, online, other) else { continue };
        let at = said.iter().position(|(told, _)| *told == node).unwrap_or_else(|| {
            said.push((node, [0; CPUS / 64]));
            said.len() - 1
        });
        said[at].1[other / 64] |= 1u64.rotate_left(u32::try_from(other).expect("cpu fits u32"));
    }
    if !said.iter().any(|(node, cpus)| *node == here && *cpus == own) {
        return None;
    }
    let mut best = own;
    for (node, cpus) in said.iter().filter(|(node, _)| *node != here) {
        let Some(share) = listed(system, *node).and_then(|list| within(allowed, list)) else { continue };
        if share == *cpus && cpu_count(&share) >= 2 * cpu_count(&own) && cpu_count(&share) > cpu_count(&best) {
            best = share;
        }
    }
    Some(best)
}

/// Bind the calling thread to one Linux NUMA node when the machine exposes
/// more than one online node and the sandbox permits `sched_setaffinity`.
pub(crate) fn confine_cli_to_one_node() {
    let system = Path::new("/sys/devices/system");
    let Some(online) = online_nodes(std::fs::read_to_string(system.join("node/online"))) else { return };
    // Sound: `sched_getcpu` takes no arguments and accesses no caller memory.
    let Ok(cpu) = usize::try_from(unsafe { libc::sched_getcpu() }) else { return };
    let Some(allowed) = affinity() else { return };
    if let Some(mask) = largest(system, &online, cpu, allowed) {
        // Sound: `mask` is `size_of::<Set>()` readable bytes aligned to `u64`.
        unsafe { libc::sched_setaffinity(0, core::mem::size_of::<Set>(), mask.as_ptr().cast()) };
    }
}
