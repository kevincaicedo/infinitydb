//! ADR-0144 D6: exercise the actual benchmark profile before timing it.

/// This entry is compiled into benchmarks only and runs outside timed code.
pub fn run_if_requested() {
    let Some(value) = std::env::var_os("INF_W03_OVERFLOW_CANARY") else {
        return;
    };
    let value = value.into_string().expect("canary argument is UTF-8");
    let value: u32 = value.parse().expect("canary argument is a u32");
    eprintln!("overflow-canary-enter={value}");
    let next = std::hint::black_box(value) + std::hint::black_box(1);
    println!("overflow-canary-result={next}");
    std::process::exit(0);
}
