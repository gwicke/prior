//! Proxy scrubbing for children spawned by the pty tests.
//!
//! Kept apart from the `common` module so a test binary that includes this
//! module uses all of it: `common` additionally holds `Dumps`, which only the
//! dump-emitting tests touch — sharing one module between the two audiences
//! leaves either `Dumps` or `clear_proxy_vars` dead in each binary.

use portable_pty::CommandBuilder;

/// Strip ambient proxy variables from a spawned child's environment.
///
/// The pty tests point the child at a mock provider on this machine's
/// loopback, which is meant to be reached directly. On a host whose
/// environment carries proxy variables but no `no_proxy` matching loopback —
/// a corporate network, or a sandbox egress plug — the child's HTTP client
/// routes the request to the proxy instead, and the proxy cannot reach the
/// test's listener: every request comes back as the proxy's `HTTP 403`/`502`
/// and the test fails for reasons unrelated to what it exercises.
/// (Reproduce on such a host: `env -u NO_PROXY -u no_proxy cargo test
/// --test esc_backoff` fails exactly that way.)
///
/// The vars are removed rather than overridden with a `no_proxy` list: an
/// override would depend on each client's list-matching rules (IPv6 forms,
/// ports, wildcards), while removal takes the direct path on any client.
/// All four names go, in both spellings, because clients disagree on which
/// they read — the same asymmetry that makes `proxy_var_specs` publish both.
pub fn clear_proxy_vars(cmd: &mut CommandBuilder) {
    for upper in ["ALL_PROXY", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY"] {
        cmd.env_remove(upper);
        cmd.env_remove(upper.to_lowercase());
    }
}
