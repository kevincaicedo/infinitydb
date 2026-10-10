use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("inf-compare-env-{}-{name}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn cpu(&self, cpu: &str, governor: &str, epp: &str) {
        let dir = self.0.join(cpu).join("cpufreq");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("scaling_governor"), governor).unwrap();
        std::fs::write(dir.join("energy_performance_preference"), epp).unwrap();
    }

    fn gather(&self) -> Env {
        gather_from(true, false, &self.0, &[])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn mixed_governors_name_the_nonzero_cpu() {
    let fixture = Fixture::new("mixed-governors");
    fixture.cpu("cpu0", "performance", "performance");
    fixture.cpu("cpu12", "powersave", "performance");
    let env = fixture.gather();
    assert!(env.reasons.iter().any(|r| r.contains("cpu12") && r.contains("powersave")), "{env:?}");
    assert!(!env.binding);
}

#[test]
fn mixed_epp_names_the_nonzero_cpu() {
    let fixture = Fixture::new("mixed-epp");
    fixture.cpu("cpu0", "performance", "performance");
    fixture.cpu("cpu2", "performance", "balance_performance");
    let env = fixture.gather();
    assert!(
        env.reasons.iter().any(|r| r.contains("cpu2") && r.contains("balance_performance")),
        "{env:?}"
    );
}

#[test]
fn missing_checker_is_a_named_refusal() {
    let fixture = Fixture::new("missing-checker");
    fixture.cpu("cpu0", "performance", "performance");
    let env = fixture.gather();
    assert!(
        env.reasons.iter().any(|r| r.contains("env-check") && r.contains("unavailable")),
        "{env:?}"
    );
}

#[test]
fn missing_epp_is_not_a_pass() {
    let fixture = Fixture::new("missing-epp");
    fixture.cpu("cpu0", "performance", "performance");
    std::fs::remove_file(fixture.0.join("cpu0/cpufreq/energy_performance_preference")).unwrap();
    let env = fixture.gather();
    assert!(env.reasons.iter().any(|r| r.contains("EPP")), "{env:?}");
}

#[test]
fn complete_sparse_cpu_scope_records_every_reading() {
    let fixture = Fixture::new("sparse");
    fixture.cpu("cpu0", "performance\n", "performance\n");
    fixture.cpu("cpu12", "performance", "performance");
    std::fs::create_dir(fixture.0.join("cpufreq")).unwrap();
    std::fs::create_dir(fixture.0.join("cpu_bad")).unwrap();
    let cpus = cpu_dirs(&fixture.0).unwrap();
    assert_eq!(cpus.len(), 2);
    let mut reasons = Vec::new();
    for (file, label) in
        [("scaling_governor", "governor"), ("energy_performance_preference", "EPP")]
    {
        let readings = cpu_policy(&cpus, file, label, &mut reasons);
        assert_eq!(readings, "cpu0=performance, cpu12=performance");
    }
    assert!(reasons.is_empty(), "{reasons:?}");
    assert!(classify(true, false, &reasons).0);
    assert!(!classify(false, false, &reasons).0);
}

#[test]
fn missing_nonzero_cpu_policy_cannot_shrink_the_scope() {
    let fixture = Fixture::new("missing-policy");
    fixture.cpu("cpu0", "performance", "performance");
    std::fs::create_dir(fixture.0.join("cpu8")).unwrap();
    let env = fixture.gather();
    assert!(env.governor.contains("cpu8=unknown"));
    assert!(env.epp.contains("cpu8=unknown"));
    for label in ["governor", "EPP"] {
        assert!(env.reasons.iter().any(|r| r.contains("cpu8") && r.contains(label)), "{env:?}");
    }
}

#[test]
fn empty_and_unreadable_policy_files_are_named_refusals() {
    let fixture = Fixture::new("unreadable");
    fixture.cpu("cpu0", "performance", "performance");
    fixture.cpu("cpu4", " \n", "performance");
    let path = fixture.0.join("cpu4/cpufreq/energy_performance_preference");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(path).unwrap();
    let env = fixture.gather();
    for label in ["governor", "EPP"] {
        assert!(
            env.reasons
                .iter()
                .any(|r| r.contains("cpu4") && r.contains(label) && r.contains("unavailable")),
            "{env:?}"
        );
    }
}

#[test]
fn empty_and_unreadable_cpu_roots_fail_closed() {
    let fixture = Fixture::new("empty");
    assert!(cpu_dirs(&fixture.0).unwrap_err().contains("no CPUs"));
    let env = gather_from(true, true, &fixture.0.join("absent"), &[]);
    assert!(env.reasons.iter().any(|r| r.contains("CPU enumeration")), "{env:?}");
    assert!(!env.binding);
    assert!(env.tier.contains("DEV-TIER"));
}

#[test]
fn unsafe_override_never_upgrades_a_failed_probe() {
    let reasons = vec!["cpu7 EPP is powersave".into()];
    for reference in [false, true] {
        let (binding, tier) = classify(reference, true, &reasons);
        assert!(!binding);
        assert!(tier.contains("non-citable"));
    }
}

#[test]
#[cfg(unix)]
fn checker_execution_requires_an_executable_successful_process() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("checker-status");
    let path = fixture.0.join("checker");
    assert_eq!(run_envcheck(std::slice::from_ref(&path)), (None, None));
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (detail, status) = run_envcheck(std::slice::from_ref(&path));
    assert_eq!(status, None);
    assert!(detail.unwrap().contains("failed to run"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(run_envcheck(std::slice::from_ref(&path)).1, Some(true));
    std::fs::write(&path, "#!/bin/sh\nexit 7\n").unwrap();
    let (detail, status) = run_envcheck(&[path]);
    assert_eq!(status, Some(false));
    assert!(detail.unwrap().contains("exit 7"));
}
