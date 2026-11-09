// SPDX-License-Identifier: MIT
// SigmaOS-irqbalance - Interrupt Affinity Balancer component
// irqbalance-style daemon distributing hardware interrupts across CPUs,
// inspired by Arch's irqbalance package.

use std::collections::HashMap;
use std::fmt;

// ============================================================================
// IRQ TYPES
// ============================================================================

/// Source of an interrupt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqType {
    /// Legacy ISA IRQ
    Isa,
    /// PCI MSI interrupt
    Msi,
    /// MSI-X interrupt
    MsiX,
    /// Message Signaled Interrupt
    MsiMsg,
    /// Legacy PIC interrupt
    Pic,
}

impl IrqType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Isa => "ISA",
            Self::Msi => "MSI",
            Self::MsiX => "MSI-X",
            Self::MsiMsg => "MSI",
            Self::Pic => "PIC",
        }
    }
}

/// An interrupt request line
#[derive(Debug, Clone)]
pub struct Interrupt {
    /// IRQ number
    pub irq: u32,
    /// Device name (e.g. "eth0", "nvme0", "xhci_hcd")
    pub device: String,
    /// Interrupt type
    pub irq_type: IrqType,
    /// Number of interrupts per second
    pub rate_per_sec: u64,
    /// CPU affinity mask (bitmask of allowed CPUs)
    pub affinity_mask: u64,
    /// Current CPU it's bound to
    pub current_cpu: Option<u32>,
    /// Whether this IRQ should be balanced
    pub balance: bool,
}

impl Interrupt {
    /// Weight of this interrupt for balancing (higher = more important)
    pub fn weight(&self) -> u64 {
        // Rate-based weight: more frequent interrupts get higher weight
        self.rate_per_sec.min(u64::MAX / 1000)
    }
}

// ============================================================================
// BALANCING ALGORITHM
// ============================================================================

/// System interrupt state
#[derive(Debug, Clone)]
pub struct InterruptState {
    pub interrupts: Vec<Interrupt>,
    pub num_cpus: u32,
}

impl InterruptState {
    /// Compute optimal affinity assignments
    pub fn balance(&self) -> Vec<(u32, u32)> {
        let mut assignments = Vec::new();
        let mut cpu_loads = vec![0u64; self.num_cpus as usize];

        // Sort interrupts by rate descending (greedy: most active first)
        let mut sorted: Vec<&Interrupt> = self.interrupts.iter()
            .filter(|i| i.balance && i.rate_per_sec > 0)
            .collect();
        sorted.sort_by(|a, b| b.rate_per_sec.cmp(&a.rate_per_sec));

        for irq in sorted {
            // Find least-loaded eligible CPU
            let best_cpu = self.find_best_cpu(irq, &cpu_loads);
            cpu_loads[best_cpu as usize] += irq.weight();
            assignments.push((irq.irq, best_cpu));
        }
        assignments
    }

    fn find_best_cpu(&self, irq: &Interrupt, loads: &[u64]) -> u32 {
        let mut best_cpu = 0u32;
        let mut best_load = u64::MAX;
        for cpu in 0..self.num_cpus {
            // Check affinity mask allows this CPU
            if irq.affinity_mask & (1u64 << cpu) == 0 {
                continue;
            }
            if loads[cpu as usize] < best_load {
                best_load = loads[cpu as usize];
                best_cpu = cpu;
            }
        }
        best_cpu
    }

    /// Compute load distribution
    pub fn load_distribution(&self, assignments: &[(u32, u32)]) -> Vec<u64> {
        let mut loads = vec![0u64; self.num_cpus as usize];
        let irq_map: HashMap<u32, &Interrupt> = self.interrupts.iter()
            .map(|i| (i.irq, i)).collect();
        for (irq_num, cpu) in assignments {
            if let Some(irq) = irq_map.get(irq_num) {
                loads[*cpu as usize] += irq.weight();
            }
        }
        loads
    }

    /// Check if balancing is needed (load imbalance > threshold)
    pub fn needs_rebalance(&self, threshold_percent: u64) -> bool {
        let assignments = self.balance();
        let loads = self.load_distribution(&assignments);
        let min = loads.iter().min().unwrap_or(&0);
        let max = loads.iter().max().unwrap_or(&0);
        if *max == 0 { return false; }
        let imbalance = (*max - *min) * 100 / *max;
        imbalance > threshold_percent
    }
}

/// Default IRQ types that should NOT be balanced (critical)
const CRITICAL_DEVICES: &[&str] = &[
    "timer", "rtc", "i8042", "keyboard", "mouse", "ps2",
];

/// Whether an IRQ should be balanced based on device
pub fn should_balance(device: &str) -> bool {
    !CRITICAL_DEVICES.iter().any(|d| device.to_lowercase().contains(d))
}

/// Default CPU affinity mask for a system with N CPUs
pub fn default_affinity(num_cpus: u32) -> u64 {
    if num_cpus >= 64 {
        u64::MAX
    } else {
        (1u64 << num_cpus) - 1
    }
}

/// Format IRQ info for display
pub fn print_irq(irq: &Interrupt) -> String {
    format!("{:>5} {:>20} {:>6} {:>10} {:>12}",
        irq.irq,
        irq.device,
        irq.irq_type.as_str(),
        irq.rate_per_sec,
        irq.current_cpu.map(|c| format!("CPU{}", c)).unwrap_or_else(|| "-".into()))
}

/// Format IRQ list for display
pub fn print_irq_list(state: &InterruptState) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:>5} {:>20} {:>6} {:>10} {:>12}\n",
        "IRQ", "Device", "Type", "Rate/s", "CPU"));
    out.push_str("----- -------------------- ------ ---------- ------------\n");
    for irq in &state.interrupts {
        out.push_str(&print_irq(irq));
        out.push('\n');
    }
    out
}

#[derive(Debug)]
pub enum IrqError {
    InvalidCpu(u32),
    InvalidIrq(u32),
    PermissionDenied,
    DeviceNotFound(String),
}

impl fmt::Display for IrqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCpu(c) => write!(f, "invalid CPU: {}", c),
            Self::InvalidIrq(i) => write!(f, "invalid IRQ: {}", i),
            Self::PermissionDenied => write!(f, "permission denied"),
            Self::DeviceNotFound(d) => write!(f, "device not found: {}", d),
        }
    }
}
impl std::error::Error for IrqError {}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_affinity_mask() {
        assert_eq!(default_affinity(4), 0b1111);
        assert_eq!(default_affinity(64), u64::MAX);
        assert_eq!(default_affinity(8), 0b11111111);
    }

    #[test]
    fn test_should_balance() {
        assert!(!should_balance("i8042"));
        assert!(!should_balance("keyboard"));
        assert!(!should_balance("timer"));
        assert!(should_balance("eth0"));
        assert!(should_balance("nvme0"));
        assert!(should_balance("xhci_hcd"));
    }

    #[test]
    fn test_balance_distribution() {
        let state = InterruptState {
            num_cpus: 4,
            interrupts: vec![
                Interrupt { irq: 24, device: "eth0".into(), irq_type: IrqType::Msi, rate_per_sec: 50000, affinity_mask: 0b1111, current_cpu: None, balance: true },
                Interrupt { irq: 25, device: "eth1".into(), irq_type: IrqType::Msi, rate_per_sec: 40000, affinity_mask: 0b1111, current_cpu: None, balance: true },
                Interrupt { irq: 26, device: "nvme0".into(), irq_type: IrqType::MsiX, rate_per_sec: 30000, affinity_mask: 0b1111, current_cpu: None, balance: true },
                Interrupt { irq: 27, device: "xhci".into(), irq_type: IrqType::Msi, rate_per_sec: 20000, affinity_mask: 0b1111, current_cpu: None, balance: true },
                Interrupt { irq: 28, device: "ahci".into(), irq_type: IrqType::Msi, rate_per_sec: 10000, affinity_mask: 0b1111, current_cpu: None, balance: true },
            ],
        };
        let assignments = state.balance();
        assert_eq!(assignments.len(), 5);
        // Should distribute across CPUs
        let loads = state.load_distribution(&assignments);
        assert!(loads.iter().any(|&l| l > 0));
    }

    #[test]
    fn test_respects_affinity() {
        let state = InterruptState {
            num_cpus: 4,
            interrupts: vec![
                Interrupt { irq: 24, device: "eth0".into(), irq_type: IrqType::Msi, rate_per_sec: 1000, affinity_mask: 0b0011, current_cpu: None, balance: true },
            ],
        };
        let assignments = state.balance();
        let cpu = assignments[0].1;
        assert!(cpu == 0 || cpu == 1); // Only CPUs 0-1 allowed
    }

    #[test]
    fn test_needs_rebalance() {
        let state = InterruptState {
            num_cpus: 4,
            interrupts: vec![
                Interrupt { irq: 24, device: "eth0".into(), irq_type: IrqType::Msi, rate_per_sec: 100000, affinity_mask: 0b0001, current_cpu: None, balance: true },
            ],
        };
        // All load on CPU0 — should need rebalance
        assert!(state.needs_rebalance(10));
    }

    #[test]
    fn test_weight() {
        let irq = Interrupt { irq: 24, device: "eth0".into(), irq_type: IrqType::Msi, rate_per_sec: 50000, affinity_mask: 0b1111, current_cpu: None, balance: true };
        assert_eq!(irq.weight(), 50000);
    }

    #[test]
    fn test_print_list() {
        let state = InterruptState {
            num_cpus: 2,
            interrupts: vec![
                Interrupt { irq: 24, device: "eth0".into(), irq_type: IrqType::Msi, rate_per_sec: 50000, affinity_mask: 0b11, current_cpu: Some(0), balance: true },
            ],
        };
        let out = print_irq_list(&state);
        assert!(out.contains("eth0"));
        assert!(out.contains("MSI"));
        assert!(out.contains("CPU0"));
    }
}
