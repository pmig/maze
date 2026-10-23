//! DNS for verification: hickory with a cache in production, fixed records in tests.
//!
//! `mail-auth` looks up TXT records (DKIM keys, DMARC policies) through its
//! [`MessageAuthenticator`] and consults a [`ResolverCache`] first. [`Resolver`]
//! owns both:
//!
//! - [`Resolver::system`] / [`Resolver::with_config`] use hickory with its own
//!   record cache (positive and negative answers, TTL-bounded) plus a bounded cache of
//!   parsed TXT records, so opening the next mail from the same sender doesn't parse
//!   the same key again.
//! - [`Resolver::from_static`] answers every lookup from a [`StaticDns`] table and
//!   never touches the network: names not in the table are NXDOMAIN.
//!
//! [`Resolver::mx_hosts`] finds an account domain's mail servers, which identify its
//! provider ([`crate::ProviderProfile::automatic`]); answers are cached the same way.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mail_auth::common::parse::TxtRecordParser;
use mail_auth::common::verify::DomainKey;
use mail_auth::dmarc::Dmarc;
use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};
use mail_auth::hickory_resolver::proto::op::ResponseCode;
use mail_auth::{DnsError, DnssecStatus, MX, MessageAuthenticator, RecordSet, ResolverCache, Txt};

/// Parsed TXT records kept by the production cache.
const TXT_CACHE_CAPACITY: usize = 4_096;
/// Upper bound on how long a parsed record is kept, whatever its TTL.
const TXT_CACHE_MAX_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// MX record sets kept by the production cache (one per account domain, in practice).
const MX_CACHE_CAPACITY: usize = 256;

/// The resolver couldn't be set up.
#[derive(Debug, thiserror::Error)]
#[error("DNS resolver setup failed: {0}")]
pub struct ResolverError(String);

/// DNS access for verification. Cheap to clone; clones share the cache.
#[derive(Clone)]
pub struct Resolver {
    pub(crate) authenticator: MessageAuthenticator,
    pub(crate) txt: TxtCache,
    pub(crate) ipv4: Ipv4Cache,
    mx: MxCache,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match &self.txt {
            TxtCache::Live(_) => "live",
            TxtCache::Static(_) => "static",
        };
        f.debug_struct("Resolver")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

impl Resolver {
    /// Use the operating system's resolver configuration (`/etc/resolv.conf` on
    /// macOS and Linux). DNS queries go where all other queries of the machine go.
    pub fn system() -> Result<Self, ResolverError> {
        let authenticator =
            MessageAuthenticator::new_system_conf().map_err(|e| ResolverError(e.to_string()))?;
        Ok(Self::live(authenticator))
    }

    /// Use an explicit hickory configuration.
    pub fn with_config(
        config: ResolverConfig,
        options: ResolverOpts,
    ) -> Result<Self, ResolverError> {
        let authenticator =
            MessageAuthenticator::new(config, options).map_err(|e| ResolverError(e.to_string()))?;
        Ok(Self::live(authenticator))
    }

    /// Answer every lookup from `records`, without any network access. For tests
    /// and offline use.
    pub fn from_static(records: StaticDns) -> Result<Self, ResolverError> {
        // No name servers: even a lookup that bypassed the cache couldn't leave
        // the machine.
        let config = ResolverConfig::from_parts(None, Vec::new(), Vec::new());
        let authenticator = MessageAuthenticator::new(config, ResolverOpts::default())
            .map_err(|e| ResolverError(e.to_string()))?;
        let records = Arc::new(records);
        Ok(Self {
            authenticator,
            txt: TxtCache::Static(Arc::clone(&records)),
            ipv4: Ipv4Cache::Static,
            mx: MxCache::Static(records),
        })
    }

    fn live(authenticator: MessageAuthenticator) -> Self {
        Self {
            authenticator,
            txt: TxtCache::Live(Arc::new(LiveCache::new(TXT_CACHE_CAPACITY))),
            ipv4: Ipv4Cache::Live,
            mx: MxCache::Live(Arc::new(LiveCache::new(MX_CACHE_CAPACITY))),
        }
    }

    /// The mail servers (MX hosts) of `domain`, most preferred first, lowercased and
    /// without the trailing dot. Empty when the domain has none.
    ///
    /// # Errors
    ///
    /// A message when the lookup fails for another reason (a timeout, say).
    pub async fn mx_hosts(&self, domain: &str) -> Result<Vec<String>, ResolverError> {
        match self
            .authenticator
            .mx_lookup(fqdn(domain).as_ref(), Some(&self.mx))
            .await
        {
            Ok(records) => Ok(records
                .rrset
                .iter()
                .flat_map(|mx| mx.exchanges.iter())
                .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect()),
            Err(mail_auth::Error::Dns(DnsError::RecordNotFound(_))) => Ok(Vec::new()),
            Err(err) => Err(ResolverError(err.to_string())),
        }
    }
}

/// Fixed DNS answers for [`Resolver::from_static`].
///
/// Records are parsed by name, the way `mail-auth` would ask for them: names
/// starting with `_dmarc.` hold DMARC policies, names containing `._domainkey.` hold
/// DKIM keys. Every name not in the table is NXDOMAIN.
#[derive(Clone, Default)]
pub struct StaticDns {
    records: HashMap<Box<str>, Txt>,
    mx: HashMap<Box<str>, Vec<String>>,
}

impl fmt::Debug for StaticDns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticDns")
            .field("records", &self.records.len())
            .field("mx", &self.mx.len())
            .finish()
    }
}

impl StaticDns {
    /// An empty table: every name is NXDOMAIN.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a TXT record. A name may get several records; the first that parses
    /// wins, as with real DNS answers.
    #[must_use]
    pub fn txt(mut self, name: &str, record: &str) -> Self {
        let name = fqdn(name);
        let parsed = parse_txt(&name, record.as_bytes());
        let keep_existing = self
            .records
            .get(&name)
            .is_some_and(|existing| !matches!(existing, Txt::Error(_)));
        if !keep_existing {
            self.records.insert(name, parsed);
        }
        self
    }

    /// Make lookups of `name` fail with a temporary resolver error (a timeout,
    /// say).
    #[must_use]
    pub fn temp_error(mut self, name: &str) -> Self {
        self.records.insert(
            fqdn(name),
            Txt::Error(mail_auth::Error::Dns(DnsError::Resolver(
                "simulated timeout".to_owned(),
            ))),
        );
        self
    }

    /// Set the MX hosts of `name`, most preferred first.
    #[must_use]
    pub fn mx<I, S>(mut self, name: &str, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.mx
            .insert(fqdn(name), hosts.into_iter().map(Into::into).collect());
        self
    }

    fn get_mx<Q>(&self, name: &Q) -> RecordSet<MX>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hosts = self.mx.get(name).cloned().unwrap_or_default();
        let rrset: Arc<[MX]> = hosts
            .into_iter()
            .zip(0u16..)
            .map(|(host, preference)| MX {
                exchanges: vec![host.into_boxed_str()].into_boxed_slice(),
                preference: preference.saturating_mul(10),
            })
            .collect();
        RecordSet {
            rrset,
            dnssec_status: DnssecStatus::Indeterminate,
        }
    }

    fn get<Q>(&self, name: &Q) -> Txt
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.records.get(name).cloned().unwrap_or_else(nxdomain)
    }
}

fn nxdomain() -> Txt {
    Txt::Error(mail_auth::Error::Dns(DnsError::RecordNotFound(
        ResponseCode::NXDomain,
    )))
}

fn fqdn(name: &str) -> Box<str> {
    let mut name = name.trim().to_ascii_lowercase();
    if !name.ends_with('.') {
        name.push('.');
    }
    name.into_boxed_str()
}

fn parse_txt(name: &str, record: &[u8]) -> Txt {
    if name.starts_with("_dmarc.") {
        Dmarc::parse(record).into()
    } else if name.contains("._domainkey.") {
        DomainKey::parse(record).into()
    } else {
        Txt::Error(mail_auth::Error::Dns(DnsError::InvalidRecordType))
    }
}

/// The TXT cache handed to `mail-auth`.
#[derive(Clone)]
pub(crate) enum TxtCache {
    Live(Arc<LiveCache>),
    Static(Arc<StaticDns>),
}

impl ResolverCache<Box<str>, Txt> for TxtCache {
    fn get<Q>(&self, name: &Q) -> Option<Txt>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self {
            Self::Live(cache) => cache.get(name),
            Self::Static(records) => Some(records.get(name)),
        }
    }

    fn remove<Q>(&self, name: &Q) -> Option<Txt>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self {
            Self::Live(cache) => cache.remove(name),
            Self::Static(_) => None,
        }
    }

    fn insert(&self, key: Box<str>, value: Txt, valid_until: Instant) {
        if let Self::Live(cache) = self {
            cache.insert(key, value, valid_until);
        }
    }
}

/// The A-record cache handed to `mail-auth`. DMARC looks up A records to tell
/// whether a subdomain exists (for the `np=` policy). In static mode every name
/// "exists" without addresses, which keeps the lookup off the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ipv4Cache {
    Live,
    Static,
}

impl ResolverCache<Box<str>, RecordSet<Ipv4Addr>> for Ipv4Cache {
    fn get<Q>(&self, _: &Q) -> Option<RecordSet<Ipv4Addr>>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self {
            Self::Static => Some(RecordSet {
                rrset: Arc::from(Vec::new()),
                dnssec_status: DnssecStatus::Indeterminate,
            }),
            Self::Live => None,
        }
    }

    fn remove<Q>(&self, _: &Q) -> Option<RecordSet<Ipv4Addr>>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        None
    }

    fn insert(&self, _: Box<str>, _: RecordSet<Ipv4Addr>, _: Instant) {}
}

/// The MX cache handed to `mail-auth`. In static mode every name is answered from
/// the table (names not in it have no MX hosts).
#[derive(Clone)]
enum MxCache {
    Live(Arc<LiveCache<RecordSet<MX>>>),
    Static(Arc<StaticDns>),
}

impl ResolverCache<Box<str>, RecordSet<MX>> for MxCache {
    fn get<Q>(&self, name: &Q) -> Option<RecordSet<MX>>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self {
            Self::Live(cache) => cache.get(name),
            Self::Static(records) => Some(records.get_mx(name)),
        }
    }

    fn remove<Q>(&self, name: &Q) -> Option<RecordSet<MX>>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self {
            Self::Live(cache) => cache.remove(name),
            Self::Static(_) => None,
        }
    }

    fn insert(&self, key: Box<str>, value: RecordSet<MX>, valid_until: Instant) {
        if let Self::Live(cache) = self {
            cache.insert(key, value, valid_until);
        }
    }
}

/// A bounded TTL cache of parsed records.
pub(crate) struct LiveCache<V = Txt> {
    capacity: usize,
    entries: Mutex<HashMap<Box<str>, (V, Instant)>>,
}

impl<V: Clone> LiveCache<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Box<str>, (V, Instant)>> {
        // A panic while holding the lock can't leave the map inconsistent.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get<Q>(&self, name: &Q) -> Option<V>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut entries = self.lock();
        match entries.get(name) {
            Some((value, expires)) if *expires > Instant::now() => Some(value.clone()),
            Some(_) => {
                entries.remove(name);
                None
            }
            None => None,
        }
    }

    fn remove<Q>(&self, name: &Q) -> Option<V>
    where
        Box<str>: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.lock().remove(name).map(|(value, _)| value)
    }

    fn insert(&self, key: Box<str>, value: V, valid_until: Instant) {
        let now = Instant::now();
        let valid_until = valid_until.min(now + TXT_CACHE_MAX_TTL);
        if valid_until <= now {
            return;
        }
        let mut entries = self.lock();
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            entries.retain(|_, (_, expires)| *expires > now);
            if entries.len() >= self.capacity {
                // Still full: drop the entry closest to expiry.
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (_, expires))| *expires)
                    .map(|(name, _)| name.clone())
                {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(key, (value, valid_until));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_nxdomain(txt: &Txt) -> bool {
        matches!(
            txt,
            Txt::Error(mail_auth::Error::Dns(DnsError::RecordNotFound(
                ResponseCode::NXDomain
            )))
        )
    }

    #[test]
    fn static_records_parse_by_name() {
        let dns = StaticDns::new()
            .txt("_dmarc.Example.com", "v=DMARC1; p=reject")
            .txt(
                "sel._domainkey.example.com.",
                "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
            )
            .txt("other.example.com", "hello")
            .temp_error("_dmarc.slow.example");
        assert!(matches!(dns.get("_dmarc.example.com."), Txt::Dmarc(_)));
        assert!(matches!(
            dns.get("sel._domainkey.example.com."),
            Txt::DomainKey(_)
        ));
        assert!(matches!(dns.get("other.example.com."), Txt::Error(_)));
        assert!(matches!(
            dns.get("_dmarc.slow.example."),
            Txt::Error(mail_auth::Error::Dns(DnsError::Resolver(_)))
        ));
        assert!(is_nxdomain(&dns.get("missing.example.")));
    }

    #[test]
    fn first_valid_record_wins() {
        let dns = StaticDns::new()
            .txt("_dmarc.example.com", "garbage")
            .txt("_dmarc.example.com", "v=DMARC1; p=none")
            .txt("_dmarc.example.com", "v=DMARC1; p=reject");
        match dns.get("_dmarc.example.com.") {
            Txt::Dmarc(record) => assert_eq!(record.p, mail_auth::dmarc::Policy::None),
            _ => panic!("expected a DMARC record"),
        }
    }

    #[test]
    fn static_cache_answers_everything() {
        let cache = TxtCache::Static(Arc::new(StaticDns::new()));
        assert!(
            cache
                .get("anything.example.")
                .is_some_and(|t| is_nxdomain(&t))
        );
        assert!(Ipv4Cache::Static.get("anything.example.").is_some());
        assert!(Ipv4Cache::Live.get("anything.example.").is_none());
    }

    #[test]
    fn live_cache_expires_and_evicts() {
        let cache = LiveCache::new(2);
        let later = Instant::now() + Duration::from_secs(60);
        cache.insert("a.".into(), nxdomain(), later);
        cache.insert("b.".into(), nxdomain(), later + Duration::from_secs(1));
        cache.insert("c.".into(), nxdomain(), later + Duration::from_secs(2));
        assert!(cache.get("a.").is_none(), "closest to expiry is evicted");
        assert!(cache.get("b.").is_some());
        assert!(cache.get("c.").is_some());

        cache.insert("expired.".into(), nxdomain(), Instant::now());
        assert!(cache.get("expired.").is_none());
        assert!(cache.remove("b.").is_some());
        assert!(cache.get("b.").is_none());
    }

    #[tokio::test]
    async fn mx_hosts_come_from_the_table() {
        let resolver = Resolver::from_static(StaticDns::new().mx(
            "Reader.Example",
            [
                "In1-SMTP.messagingengine.com.",
                "in2-smtp.messagingengine.com",
            ],
        ))
        .unwrap();
        assert_eq!(
            resolver.mx_hosts("reader.example").await.unwrap(),
            [
                "in1-smtp.messagingengine.com",
                "in2-smtp.messagingengine.com"
            ]
        );
        assert!(resolver.mx_hosts("none.example").await.unwrap().is_empty());
    }

    #[test]
    fn static_resolver_builds_without_runtime() {
        let resolver = Resolver::from_static(StaticDns::new()).unwrap();
        assert_eq!(format!("{resolver:?}"), "Resolver { mode: \"static\", .. }");
    }
}
