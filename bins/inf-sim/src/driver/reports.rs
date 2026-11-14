//! Every scenario's report as the drivers read it ([`SeedReport`]). A
//! report's verdict is its own `ok()`, with a legal boot refusal where the
//! report records one.

use super::{SeedReport, SimSeconds, Verdict, Violations};
use crate::{
    BackfillReport, BootStormReport, ColdStormReport, CombinedReport, DiskfullReport,
    DurableReport, ExtentWindowReport, NsCreateWindowReport, NsDdlRaceReport, NsDropWindowReport,
    NsLifetimesReport, PressureReport, RecoveryReport, SidecarReport, SimReport, SteelReport,
    TieredNodeReport, WakeReport,
};

impl SeedReport for SimReport {
    type Readings = ();

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.ok(), false, &self.oracle_violations)
    }
    fn violations(&self) -> &Violations {
        &self.oracle_violations
    }
    fn trace(&self) -> Option<&[u8]> {
        Some(&self.trace)
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        Some(SimSeconds {
            seconds: self.sim_seconds,
            published: self.published,
            delivered: self.delivered,
        })
    }
    fn readings(&self) {}
}

/// The durable family's twin reproduces its wake readings, which the trace
/// does not carry.
impl SeedReport for DurableReport {
    type Readings = WakeReport;

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.ok(), self.refused_boot, &self.violations)
    }
    fn violations(&self) -> &Violations {
        &self.violations
    }
    fn trace(&self) -> Option<&[u8]> {
        Some(&self.trace)
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        Some(SimSeconds { seconds: self.sim_seconds, published: 0, delivered: 0 })
    }
    fn readings(&self) -> WakeReport {
        self.wake
    }
}

// The durable family's readings are its wake readings, and no other type: a
// report whose readings were `()` would drop the wake comparison with no
// red, so this does not build unless they are `WakeReport`.
const _: fn(&DurableReport) -> WakeReport = <DurableReport as SeedReport>::readings;

impl SeedReport for CombinedReport {
    type Readings = ();

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.ok(), self.refused_boot, &self.violations)
    }
    fn violations(&self) -> &Violations {
        &self.violations
    }
    fn trace(&self) -> Option<&[u8]> {
        Some(&self.trace)
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        Some(SimSeconds {
            seconds: self.sim_seconds,
            published: self.published,
            delivered: self.delivered,
        })
    }
    fn readings(&self) {}
}

impl SeedReport for TieredNodeReport {
    type Readings = ();

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.ok(), self.refused_boot, &self.violations)
    }
    fn violations(&self) -> &Violations {
        &self.violations
    }
    fn trace(&self) -> Option<&[u8]> {
        Some(&self.trace)
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        Some(SimSeconds { seconds: self.sim_seconds, published: 0, delivered: 0 })
    }
    fn readings(&self) {}
}

/// The reports that keep their full trace and measure virtual time, with
/// no pub/sub counts and no legal refusal.
macro_rules! traced {
    ($($report:ty),+ $(,)?) => {$(
        impl SeedReport for $report {
            type Readings = ();

            fn trace_hash(&self) -> u64 {
                self.trace_hash
            }
            fn state_hash(&self) -> u64 {
                self.state_hash
            }
            fn verdict(&self) -> Verdict {
                Verdict::judge(self.ok(), false, &self.violations)
            }
            fn violations(&self) -> &Violations {
                &self.violations
            }
            fn trace(&self) -> Option<&[u8]> {
                Some(&self.trace)
            }
            fn sim_seconds(&self) -> Option<SimSeconds> {
                Some(SimSeconds { seconds: self.sim_seconds, published: 0, delivered: 0 })
            }
            fn readings(&self) {}
        }
    )+};
}

traced!(NsDdlRaceReport, NsCreateWindowReport, NsDropWindowReport, NsLifetimesReport);

impl SeedReport for ExtentWindowReport {
    type Readings = ();

    fn trace_hash(&self) -> u64 {
        self.trace_hash
    }
    fn state_hash(&self) -> u64 {
        self.state_hash
    }
    fn verdict(&self) -> Verdict {
        Verdict::judge(self.ok(), false, &self.violations)
    }
    fn violations(&self) -> &Violations {
        &self.violations
    }
    fn trace(&self) -> Option<&[u8]> {
        Some(&self.trace)
    }
    fn sim_seconds(&self) -> Option<SimSeconds> {
        None
    }
    fn readings(&self) {}
}

/// The reports that keep no full trace and measure no virtual time: the
/// twin compares their hashes and their verdict.
macro_rules! hashed {
    ($($report:ty),+ $(,)?) => {$(
        impl SeedReport for $report {
            type Readings = ();

            fn trace_hash(&self) -> u64 {
                self.trace_hash
            }
            fn state_hash(&self) -> u64 {
                self.state_hash
            }
            fn verdict(&self) -> Verdict {
                Verdict::judge(self.ok(), false, &self.violations)
            }
            fn violations(&self) -> &Violations {
                &self.violations
            }
            fn trace(&self) -> Option<&[u8]> {
                None
            }
            fn sim_seconds(&self) -> Option<SimSeconds> {
                None
            }
            fn readings(&self) {}
        }
    )+};
}

hashed!(
    SteelReport,
    BackfillReport,
    SidecarReport,
    DiskfullReport,
    PressureReport,
    RecoveryReport,
    ColdStormReport,
    BootStormReport,
);
