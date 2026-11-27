//! Linux NUMA node confinement for the CLI process.
//!
//! Keeps `b3sum`'s main thread on one memory node before it starts its file
//! readers and hash workers, where `/sys/devices/system` exposes more than
//! one online node and `sched_setaffinity` allows the binding. Mirrors
//! `blake3-servil`'s worker-pool node selection (8,192-CPU sets and
//! `/proc/thread-self/status` fallback when a sandbox refuses
//! `sched_getaffinity`) without requiring any undocumented hook in
//! `blake3-servil`.

use std::path::Path;

/// The most CPUs a set names: the most any Linux kernel is built for
/// (x86-64's 8192). A smaller set fails with `EINVAL` where the kernel has
/// more possible CPUs than it names, whichever CPUs the process is allowed.
const CPUS: usize = 8192;
type Set = [u64; CPUS / 64];

fn has(set: &Set, cpu: usize) -> bool {
    set.get(cpu / 64).is_some_and(|word| word & 1u64.rotate_left(u32::try_from(cpu).expect("cpu fits u32")) != 0)
}

/// A number as the kernel writes one (decimal digits, no sign, no leading
/// zero before another digit) that a set has a place for.
fn number(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse::<usize>().ok().filter(|&n| n < CPUS)
}

/// Parse a Linux kernel CPU/node list (e.g. `0-3,8,10-11,4095-4096,8191`).
fn cpu_set(list: &str) -> Option<Set> {
    let mut set = [0u64; CPUS / 64];
    for range in list.trim().split(',') {
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let (first, last) = (number(first)?, number(last)?);
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

fn allowed_from_status(status: &str) -> Option<Set> {
    status.lines().find_map(|line| line.strip_prefix("Cpus_allowed_list:")).and_then(cpu_set)
}

fn affinity_with(read_affinity: impl FnOnce() -> std::io::Result<Set>, read_status: impl FnOnce() -> std::io::Result<String>) -> std::io::Result<Set> {
    match read_affinity() {
        Ok(set) => Ok(set),
        Err(err) => read_status().ok().as_deref().and_then(allowed_from_status).ok_or(err),
    }
}

fn affinity_syscall() -> std::io::Result<Set> {
    let mut set = [0u64; CPUS / 64];
    // Sound: `set` is `size_of::<Set>()` writable bytes aligned to `u64` (at least `cpu_set_t` alignment).
    if unsafe { libc::sched_getaffinity(0, core::mem::size_of::<Set>(), set.as_mut_ptr().cast()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(set)
}

fn affinity() -> std::io::Result<Set> {
    affinity_with(affinity_syscall, || std::fs::read_to_string("/proc/thread-self/status"))
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

fn current_node_mask() -> Option<Set> {
    #[cfg(test)]
    if let Some(pretended) = test::PRETENDED_NODE.get() {
        return Some(*pretended);
    }
    let system = Path::new("/sys/devices/system");
    let online = online_nodes(std::fs::read_to_string(system.join("node/online")))?;
    // Sound: `sched_getcpu` takes no arguments and accesses no caller memory.
    let cpu = usize::try_from(unsafe { libc::sched_getcpu() }).ok()?;
    let allowed = affinity().ok()?;
    largest(system, &online, cpu, allowed)
}

/// Bind the calling thread to one Linux NUMA node when the machine exposes
/// more than one online node and the sandbox permits `sched_setaffinity`.
pub(crate) fn confine_cli_to_one_node() {
    if let Some(mask) = current_node_mask() {
        // Sound: `mask` is `size_of::<Set>()` readable bytes aligned to `u64`.
        unsafe { libc::sched_setaffinity(0, core::mem::size_of::<Set>(), mask.as_ptr().cast()) };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::OnceLock;

    pub(super) static PRETENDED_NODE: OnceLock<Set> = OnceLock::new();

    fn bits(set: &Set) -> Vec<usize> {
        (0..CPUS).filter(|&cpu| has(set, cpu)).collect()
    }

    #[test]
    fn cpu_set_handles_full_8192_cpu_mask_and_rejects_malformed() {
        let set = cpu_set("0,1024,4095-4096,8191\n").expect("8192-CPU list parses");
        assert_eq!(bits(&set), vec![0, 1024, 4095, 4096, 8191]);
        assert!(cpu_set("8192").is_none());
        assert!(cpu_set("01").is_none());
        assert!(cpu_set("4-2").is_none());
        assert!(cpu_set("").is_none());
        assert!(cpu_set("+1").is_none());
    }

    #[test]
    fn affinity_falls_back_to_proc_status_when_syscall_is_refused() {
        let set = affinity_with(
            || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            || Ok("Name:\tb3sum\nCpus_allowed_list:\t0,1024,4095-4096,8191\n".to_string()),
        )
        .expect("proc status fallback parses full mask");
        assert_eq!(bits(&set), vec![0, 1024, 4095, 4096, 8191]);
    }

    #[test]
    fn largest_keeps_current_node_unless_other_has_at_least_twice_as_many_cpus() {
        let dir = crate::tempfile::tempdir().unwrap();
        let sys = dir.path();
        std::fs::create_dir_all(sys.join("node/node0")).unwrap();
        std::fs::create_dir_all(sys.join("node/node1")).unwrap();
        std::fs::write(sys.join("node/online"), "0-1\n").unwrap();
        // Node 0: CPUs 0..=1 (2 CPUs); Node 1: CPUs 2..=5 (4 CPUs).
        std::fs::write(sys.join("node/node0/cpulist"), "0-1\n").unwrap();
        std::fs::write(sys.join("node/node1/cpulist"), "2-5\n").unwrap();
        for c in 0..=1 {
            std::fs::create_dir_all(sys.join(format!("cpu/cpu{c}/node0"))).unwrap();
        }
        for c in 2..=5 {
            std::fs::create_dir_all(sys.join(format!("cpu/cpu{c}/node1"))).unwrap();
        }
        let online = online_nodes(std::fs::read_to_string(sys.join("node/online"))).unwrap();

        // Allowed = 0-4 -> node0 has 2 CPUs, node1 has 3 CPUs (< 2*2): keep current node0.
        let allowed_less_than_2x = cpu_set("0-4").unwrap();
        let stay = largest(sys, &online, 0, allowed_less_than_2x).expect("confinement mask");
        assert_eq!(bits(&stay), vec![0, 1], "must keep current node when other node has < 2x allowed CPUs (M25)");

        // Allowed = 0-5 -> node0 has 2 CPUs, node1 has 4 CPUs (>= 2*2): move to node1.
        let allowed_at_least_2x = cpu_set("0-5").unwrap();
        let switch = largest(sys, &online, 0, allowed_at_least_2x).expect("confinement mask");
        assert_eq!(bits(&switch), vec![2, 3, 4, 5], "must switch to other node with >= 2x allowed CPUs (M25)");
    }

    #[test]
    fn the_cli_mask_binds_to_a_pretended_node() {
        std::thread::spawn(|| {
            let before = affinity().unwrap();
            let cpus = bits(&before);
            if cpus.len() < 4 {
                return;
            }
            // Sound: `before` is `size_of::<Set>()` readable bytes.
            if unsafe { libc::sched_setaffinity(0, core::mem::size_of::<Set>(), before.as_ptr().cast()) } != 0 {
                let err = std::io::Error::last_os_error();
                if matches!(err.raw_os_error(), Some(libc::EPERM | libc::EACCES)) {
                    return;
                }
                panic!("sched_setaffinity failed unexpectedly: {err}");
            }
            let half_cpus: Vec<usize> = cpus.iter().copied().step_by(2).collect();
            let half_list = half_cpus.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
            let half_mask = cpu_set(&half_list).unwrap();
            assert!(PRETENDED_NODE.set(half_mask).is_ok());
            confine_cli_to_one_node();
            assert_eq!(affinity().unwrap(), half_mask, "CLI confinement binds caller to the chosen node");
        })
        .join()
        .unwrap();
    }
}
