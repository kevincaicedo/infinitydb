//! Pre-effect refusals (ADR-0172): a reservation refused before any
//! effect answers one of four classes, and each class is one RESP prefix
//! and one suffix around a fixed domain token. The classes, the domains and
//! every rendered line come from the one table below, so a client tells a
//! bounded resource that returns on its own (`TRYAGAIN`) from a limit or a
//! spent identity space (`ERR`) and a refused reservation (`OOM`) by the
//! prefix, and the last two apart by the suffix. A domain token is a fixed
//! lower-case word, never client bytes, so a rendered line needs no
//! sanitizing (ADR-0097). A server site returns the typed value from its
//! admission step and renders it once; no site writes the text.

/// Builds [`Refusal`], [`Domain`] and [`Refusal::render`] from the one
/// table: each class's variant, prefix and suffix, and each domain's
/// variant and token. Every rendered line is joined from them at compile
/// time, so a line can differ from its table row nowhere.
macro_rules! refusal_table {
    (
        classes: [$($(#[$class_doc:meta])* $class:ident => $prefix:literal, $suffix:literal;)*]
        domains: $domains:tt
    ) => {
        /// A reservation refused before any effect: its class decides the
        /// reply's prefix and suffix, its [`Domain`] the token between.
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        pub enum Refusal {
            $($(#[$class_doc])* $class(Domain),)*
        }

        refusal_table!(@domains $domains);

        impl Refusal {
            /// The refusal's text, the RESP error line without its `-` and
            /// its line end: `<prefix> <domain token> <suffix>`.
            #[must_use]
            pub const fn render(self) -> &'static str {
                match self {
                    $(Refusal::$class(domain) => {
                        refusal_table!(@line domain, $prefix, $suffix, $domains)
                    })*
                }
            }

            /// The domain the refusal names.
            #[must_use]
            pub const fn domain(self) -> Domain {
                match self {
                    $(Refusal::$class(domain) => domain,)*
                }
            }

            /// Every refusal the table renders: each class in each domain.
            pub fn every() -> impl Iterator<Item = Refusal> {
                Domain::ALL.iter().flat_map(|&domain| [$(Refusal::$class(domain),)*])
            }
        }
    };
    (@domains [$($(#[$domain_doc:meta])* $domain:ident => $token:literal;)*]) => {
        /// The resource a [`Refusal`] names, rendered as its fixed token.
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        pub enum Domain {
            $($(#[$domain_doc])* $domain,)*
        }

        impl Domain {
            /// Every domain, in the table's order.
            pub const ALL: &'static [Domain] = &[$(Domain::$domain,)*];

            /// The domain's token, as every rendered line spells it.
            #[must_use]
            pub const fn token(self) -> &'static str {
                match self {
                    $(Domain::$domain => $token,)*
                }
            }
        }
    };
    (@line $domain:ident, $prefix:literal, $suffix:literal,
        [$($(#[$domain_doc:meta])* $name:ident => $token:literal;)*]) => {
        match $domain {
            $(Domain::$name => concat!($prefix, " ", $token, " ", $suffix),)*
        }
    };
}

refusal_table! {
    classes: [
        /// A bounded resource is in use and returns without operator action:
        /// `TRYAGAIN <domain> capacity in use`.
        CapacityBusy => "TRYAGAIN", "capacity in use";
        /// A product or configured limit: retrying cannot succeed until state
        /// or configuration changes. `ERR <domain> limit reached`.
        LimitReached => "ERR", "limit reached";
        /// Memory, a quota or a native allocation refused before any effect:
        /// `OOM <domain> reservation refused`.
        ResourceRefused => "OOM", "reservation refused";
        /// A lifetime identity space is spent, permanently for its data
        /// directory or boot: `ERR <domain> identity space exhausted`.
        IdentityExhausted => "ERR", "identity space exhausted";
    ]
    domains: [
        /// A node's tiered namespace lifetimes.
        TieredNamespace => "tiered namespace";
        /// A data directory's namespace identities.
        Namespace => "namespace";
        /// The node catalog's entries.
        Catalog => "catalog";
        /// A boot's checkpoint identities.
        Checkpoint => "checkpoint";
        /// A secondary index's resources.
        Index => "index";
        /// A store's resources.
        Store => "store";
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes the tree already answers keep their spelling: the
    /// checkpoint quota's exhaustion, and the namespace identity space's
    /// (the owner's ruling of the namespace domain's token).
    #[test]
    fn the_rendered_lines_keep_the_tree_bytes() {
        let checkpoint = Refusal::IdentityExhausted(Domain::Checkpoint);
        assert_eq!(checkpoint.render(), "ERR checkpoint identity space exhausted");
        let namespace = Refusal::IdentityExhausted(Domain::Namespace);
        assert_eq!(namespace.render(), "ERR namespace identity space exhausted");
        let busy = Refusal::CapacityBusy(Domain::TieredNamespace);
        assert_eq!(busy.render(), "TRYAGAIN tiered namespace capacity in use");
        let limit = Refusal::LimitReached(Domain::TieredNamespace);
        assert_eq!(limit.render(), "ERR tiered namespace limit reached");
        assert_eq!(Refusal::LimitReached(Domain::Catalog).render(), "ERR catalog limit reached");
    }

    /// Every line is one RESP error line: a Redis error prefix, the domain's
    /// token and the class's suffix, printable ASCII with no line break, and
    /// no two refusals render alike.
    #[test]
    fn every_line_is_its_prefix_its_token_and_its_suffix() {
        let mut lines = Vec::new();
        for refusal in Refusal::every() {
            let line = refusal.render();
            let (prefix, suffix) = match refusal {
                Refusal::CapacityBusy(_) => ("TRYAGAIN ", " capacity in use"),
                Refusal::LimitReached(_) => ("ERR ", " limit reached"),
                Refusal::ResourceRefused(_) => ("OOM ", " reservation refused"),
                Refusal::IdentityExhausted(_) => ("ERR ", " identity space exhausted"),
            };
            let token = line.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix));
            assert_eq!(token, Some(refusal.domain().token()), "{refusal:?} renders {line:?}");
            assert!(line.bytes().all(|b| b.is_ascii_graphic() || b == b' '), "{line:?}");
            assert!(!lines.contains(&line), "two refusals render {line:?}");
            lines.push(line);
        }
        assert_eq!(lines.len(), 4 * Domain::ALL.len(), "each class in each domain");
    }
}
