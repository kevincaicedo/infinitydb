#![allow(dead_code)]
#[path = "after/bins/inf-sim/src/txmodel.rs"]
mod after;
#[path = "before/bins/inf-sim/src/txmodel.rs"]
mod before;
fn main() {
    for mask in 0..8 {
        let old = before::revision::Rules {
            reincarnate_on_wrap: mask & 1 != 0,
            resume_from_durable_reservation: mask & 2 != 0,
            refuse_at_exhaustion: mask & 4 != 0,
        };
        let new = after::revision::Rules {
            reincarnate_on_wrap: mask & 1 != 0,
            resume_from_durable_reservation: mask & 2 != 0,
            refuse_at_exhaustion: mask & 4 != 0,
        };
        for seed in 1..=5000 {
            assert_eq!(
                format!(
                    "{:?}",
                    before::revision::run(old, &before::revision::random_steps(seed, 256))
                ),
                format!(
                    "{:?}",
                    after::revision::run(new, &after::revision::random_steps(seed, 256))
                ),
                "revision mask={mask} seed={seed}"
            );
        }
    }
    for mask in 0..4 {
        let old = before::identity::Rules {
            resume_from_durable_reservation: mask & 1 != 0,
            checkpoint_carries_reservation: mask & 2 != 0,
        };
        let new = after::identity::Rules {
            resume_from_durable_reservation: mask & 1 != 0,
            checkpoint_carries_reservation: mask & 2 != 0,
        };
        for seed in 1..=5000 {
            assert_eq!(
                format!(
                    "{:?}",
                    before::identity::run(old, &before::identity::random_steps(seed, 256))
                ),
                format!(
                    "{:?}",
                    after::identity::run(new, &after::identity::random_steps(seed, 256))
                ),
                "identity mask={mask} seed={seed}"
            );
        }
    }
    println!(
        "PASS: 40,000 revision and 20,000 identity histories; all rule combinations; \
         identical results, statistics and error text between the supplied revisions."
    );
}
