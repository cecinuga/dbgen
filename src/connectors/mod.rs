//! The connectors: one module per database, and the registry that lists them.
//!
//! | Connector | Feature | DDL adapter | Bulk format | Batch atomicity |
//! |---|---|---|---|---|
//! | [`clickhouse`] | `clickhouse` | own, token-level | `INSERT ... FORMAT RowBinary` over HTTP | one request |
//! | [`postgres`] | `postgres` | shared ([`crate::dialect::standard`]) | `COPY ... FROM STDIN`, text format | one `COPY` statement |
//! | [`mysql`] | `mysql` | shared | multi-row `INSERT` statements | one transaction |
//! | [`sqlite`] | `sqlite` | shared | multi-row `INSERT` statements | one transaction, one writer |
//!
//! Adding a database means adding a module here that defines a [`Connector`] and a
//! [`Backend`](crate::db::Backend), one line in the `connectors!` list below, and a Cargo
//! feature of the same name for its driver. Nothing else in dbgen names a database.
//! `template.rs` is a commented starting point; `docs/CONNECTORS.md` is the guide.
//!
//! [`Connector`]: crate::connector::Connector

use crate::connector::Dialect;

/// Declares every connector module, each behind the Cargo feature named first, and the
/// [`REGISTRY`] of their `CONNECTOR` handles in the same order.
macro_rules! connectors {
    ($($feature:literal => $module:ident),* $(,)?) => {
        $(
            #[cfg(feature = $feature)]
            pub mod $module;
        )*

        /// Every connector in this build. The order is the order of `--help` and of
        /// messages, and decides which connector a URL scheme selects if two claim it (the
        /// registry tests forbid that).
        pub static REGISTRY: &[Dialect] = &[$(
            #[cfg(feature = $feature)]
            $module::CONNECTOR,
        )*];
    };
}

// The registry: one line per connector.
connectors! {
    "clickhouse" => clickhouse,
    "postgres" => postgres,
    "mysql" => mysql,
    "sqlite" => sqlite,
}

/// A commented connector to copy; compiled with the tests so it keeps up with the API, and
/// never registered.
#[cfg(test)]
mod template;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_aliases_and_schemes_are_unique_and_lowercase() {
        let mut names = Vec::new();
        let mut schemes = Vec::new();
        for d in REGISTRY {
            for n in std::iter::once(&d.name()).chain(d.aliases()) {
                assert_eq!(*n, n.to_ascii_lowercase(), "{} names itself {n}", d.product());
                assert!(!names.contains(n), "{n} is claimed twice");
                names.push(*n);
            }
            assert!(!d.schemes().is_empty(), "{} claims no URL scheme", d.product());
            for s in d.schemes() {
                assert_eq!(*s, s.to_ascii_lowercase(), "{} claims scheme {s}", d.product());
                assert!(!schemes.contains(s), "scheme {s} is claimed twice");
                schemes.push(*s);
            }
        }
    }

    #[test]
    fn handles_resolve_and_round_trip() {
        for &d in REGISTRY {
            assert_eq!(Dialect::from_name(d.name()), Some(d));
            assert_eq!(Dialect::from_name(&d.name().to_uppercase()), Some(d));
            for a in d.aliases() {
                assert_eq!(Dialect::from_name(a), Some(d));
            }
            for s in d.schemes() {
                assert_eq!(Dialect::from_scheme(s), Some(d));
            }
            let json = serde_json::to_string(&d).unwrap();
            assert_eq!(json, format!("\"{}\"", d.name()));
            assert_eq!(serde_json::from_str::<Dialect>(&json).unwrap(), d);
            assert_eq!(<Dialect as clap::ValueEnum>::from_str(d.name(), false), Ok(d));
        }
        assert!(Dialect::from_name("no-such-database").is_none());
        assert!(Dialect::from_scheme("no-such-scheme").is_none());
    }

    /// Identifiers a connector quotes read back as one quoted identifier in its dialect:
    /// spaces, dots, keywords, case and double quotes survive. (Backticks are left out:
    /// `sqlparser` does not read ClickHouse's backslash escapes, which the server does.)
    #[test]
    fn quoted_identifiers_tokenize_as_one_identifier() {
        use sqlparser::tokenizer::{Token, Tokenizer};
        for d in REGISTRY {
            for name in ["plain", "with space", "dot.ted", "q\"uote", "MixedCase", "select"] {
                let quoted = d.quote_ident(name);
                let tokens = Tokenizer::new(d.sql(), &quoted).tokenize().unwrap_or_else(|e| panic!("{}: {quoted}: {e}", d.name()));
                match tokens.as_slice() {
                    [Token::Word(w)] => assert!(w.quote_style.is_some(), "{}: {quoted} is not quoted", d.name()),
                    other => panic!("{}: {quoted} tokenizes as {other:?}", d.name()),
                }
            }
        }
    }
}
