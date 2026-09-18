# Launch operations

Supported v1 topology: one application process, PostgreSQL 17, one internet-facing
HTTPS nginx edge on the application host, and SPA/API on the same origin. The
backend listens only on loopback. `ops/nginx.conf.example` is a template, **not an
accepted production deployment**. No CDN, load-balancer chain or extra application
replica is covered by this topology. Those require a new acceptance run; replicas
also require shared admission/auth limits and cross-process session invalidation.

## Edge identity and admission

Set `APP_ENV=production`, `BIND_ADDR=127.0.0.1:3000`, the exact HTTPS
`FRONTEND_ORIGIN`, its deliberate `RP_ID`, authenticated STARTTLS SMTP, and
`TRUSTED_PROXY_IPS=127.0.0.1`. Keep the backend port inaccessible from the internet.
Trust exact immediate peer addresses only, never arbitrary client networks.

The edge **overwrites** `X-Forwarded-For` with `$remote_addr`; it must not use
`$proxy_add_x_forwarded_for`. The backend accepts exactly one IP in exactly one
header from a trusted peer. Missing, repeated, malformed, port-bearing and
comma-separated values return `400 invalid_client_ip`. It ignores `Forwarded`
and `X-Real-IP`. An untrusted peer's forwarding headers are ignored entirely.
A missing socket extension returns `500 internal_error`, not a shared anonymous
bucket. IPv4-mapped IPv6 is canonicalized. The verified full IP reaches Doorman's
limits, session context and audit sink via its typed `ClientIp` extension.

The template disables nginx's request error log because its messages can include
full query strings even when the access-log format omits them. Monitor access-log
status/latency and application error events; debug with sanitized staging traffic.
It forces no-store on edge errors as well as proxied responses. Static asset
caching can be configured separately once the final frontend deployment is known.
`python3 scripts/check-proxy.py` tests the template locally with temporary TLS,
a mock upstream and Docker; it checks spoof resistance, non-caching, body limits
and token-free access logs. It never binds a public interface or uses production
certificates. CI runs this smoke test and the separate fixture restore drill.

Do not enable nginx `real_ip` rewriting or put a CDN in front of this template:
that changes the trust boundary. Verify with `nginx -t` and two external clients
before enabling traffic. The official [nginx proxy header documentation](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_set_header)
describes the overwrite behavior used here.

Application token buckets apply before session lookup, provider access and body
parsing. A bucket starts with its burst and refills continuously; concurrent
requests hold slots until their handler returns or its future is dropped.
Rejected/invalid requests do not refund spent tokens. Limits are shared across
router clones and reset only on process restart.

| Traffic, per client | Burst | Refill per minute | Concurrent handlers |
| --- | ---: | ---: | ---: |
| All requests | 600 | 600 | 32 |
| `/api/catalog/*`, including search, detail, import and HEAD | 5 | 30 | 2 |
| Every `/api/*` POST/PUT/PATCH/DELETE | 120 | 120 | 8 |
| Export, import preview and apply combined | 2 | 6 | 1 |
| Report submission | 5 | 10 | 2 |

All applicable buckets must admit a request. Public browsing/feed reads have the
general budget; publication, follows, blocks and operator decisions have the write
budget too. Report creation also retains its database-enforced 20-new-cases per
account per rolling day, and Doorman retains its authentication-specific limits.
IPv4 clients share an IP bucket; native IPv6 uses /64 to prevent interface-address
rotation. Users behind a NAT share budgets. Distributed abuse still requires edge
controls, capacity monitoring and operator intervention.

The registry holds at most 50,000 client/class entries. Idle entries expire after
ten minutes and are swept at most once a minute during traffic. It never evicts
live limits to admit new identities. At capacity, new identities fail closed for
60 seconds while existing identities keep their budgets. `429 rate_limited`
includes integer `Retry-After` seconds and `Cache-Control: no-store`. Token waits
reflect the time until one token; concurrency waits suggest one second, with no
reservation guarantee. Keyed writes retry with the same key and original input.
Do not queue immediate retries. Health probes and CORS preflight are exempt from
application limits; bound those at the edge. Keep the existing global IGDB queue,
concurrency limit and provider cooldowns as additional protections.

## Health, logs and alerts

- `/health/live`: process responsiveness, no database lookup.
- `/health/ready`: bounded database pool acquisition and `SELECT 1`; 503 on failure.
  IGDB/email outages do not make stored diaries unavailable and are not readiness
  dependencies. Restrict frequent probes to the edge/private monitor.
- Structured `Request completed` events include route template, status and elapsed
  milliseconds. Aggregate request rate, p50/p95/p99 latency and 5xx/429 counts;
  `Request admission rejected` includes the limit class, without IPs or content.
- Alert on readiness failures, sustained 5xx, IGDB errors/queue saturation,
  authentication email/audit failures, cleanup errors, disk/DB pool pressure,
  replication count other than one, missing backup checksums and restore failures.
  `Maintenance cycle completed` marks a five-minute cycle; inspect cleanup error
  events, since completion alone does not mean every cleanup succeeded.
- Initial objectives: alert after three failed readiness probes, any audit/backup/
  cleanup failure, a backup age over 26 hours, or a restore drill age over 35 days.
  Calibrate latency/error thresholds using actual acceptance/load measurements.
- Keep application/edge logs for 14 days, restricted to operators on encrypted
  storage. Do not log query strings, request/response bodies, cookies or auth
  headers. Doorman info spans include private user/session IDs: protect logs
  accordingly. Never enable terminal email in production. Verification tokens in
  signup/recovery/email URLs must not enter proxy, analytics or error-report logs.
- Disable edge/CDN caching for `/api`, `/auth` and `/health`, including error
  responses. Do not override `no-store`, generate validators or reuse a cached
  public profile/feed after revocation. In-flight/already-downloaded responses
  cannot be recalled; subsequent requests must consult live permissions.

## Retention and deletion

| Data | Live retention |
| --- | --- |
| Authentication audit events, including IPs/identifiers | 90 days; five-minute deletion job |
| Moderation evidence/report details and free-text audit reasons | 90 days; reads suppress expired text immediately and maintenance scrubs it; reported-account deletion scrubs immediately |
| Minimal moderation case/action metadata | Private operational record until an explicit policy change; never export as personal diary data |
| Original successful request receipt body/ETag | Seven days from commit; immediate read expiry, physical cleanup every five minutes |
| Expired request-key tombstones and import provenance | Until account deletion; do not prune as ordinary expired data |
| Expired sessions and challenges | Doorman five-minute purge |
| Backups | At most 30 days, including replicas, offsite copies and object versions |

Ordinary item deletion may leave its private receipt acknowledgement for the
remainder of seven days. Account deletion cascades receipts/provenance and personal
content. Authentication audit records can retain identifiers until the 90-day
cutoff. Backup copies can retain pre-deletion data until their 30-day expiry; state
this honestly in the service's retention notice. Audit writes are best-effort and
logged on failure, so they are not a guaranteed compliance ledger.

## Backups and restore

Policy: a daily whole-database custom-format dump, authenticated encrypted off-host
copy, maximum 30-day object lifetime, monthly restore drill and a drill before each
schema-changing deployment. Target RPO is 24 hours; target RTO is four hours,
**unproven until timed on the actual data size and hosting system**. Provision the
scheduler, backup role/secret, encrypted destination, off-host copy and alerts in
the actual environment before launch. The `ops/gameboxd-backup.service` and `.timer` templates schedule the local dump;
install them only after provisioning their user, paths, secret environment and
failure monitoring, plus the separate offsite upload/30-day expiry jobs.
Neither a template nor a local drill proves
that a production scheduler or offsite recovery works.

Use PostgreSQL 17 client tools (or a deliberately tested newer version), Python 3
and `sha256sum`. Authenticate using restricted `PGPASSFILE`/`PGSERVICE` credentials;
avoid passwords/URLs in command arguments or logs. The backup role needs read
access to every table in both schemas, including authentication. The restore role
needs permission to create isolated databases and extensions. Preserve role grants,
SMTP/provider credentials and nginx configuration separately in the secret/config
backup system; `--no-owner --no-acl` deliberately does not reproduce privileges.

```sh
# PGHOST/PGPORT/PGUSER/PGPASSFILE are supplied by the scheduler secret environment.
PGDATABASE=gameboxd scripts/backup.sh /encrypted/gameboxd-backups
PGDATABASE=postgres scripts/restore-check.sh /encrypted/gameboxd-backups/gameboxd-...dump
```

`backup.sh` uses one consistent whole-database snapshot containing application and
authentication tables, both migration histories, constraints and sequences. It
writes with private permissions, checks the archive index, then publishes the
archive and SHA-256 manifest. Only pairs with a verified manifest count as completed
backups; interrupted/unpaired files must not count as success. SHA-256 detects
corruption, not malicious replacement: protect and authenticate the backup store.
Upload both files, verify the remote checksum, then record success. Alert on any
failure and on incomplete uploads; retention must expire copies and object versions.
The script does not delete old backups or pretend an offsite copy was made.

`restore-check.sh` verifies the checksum before creating a randomly named empty
database, restores atomically with fail-on-error, checks migration histories and
validated constraints, reads public views, and drops only the database it created.
It never restores over the database in `PGDATABASE`. An optional executable
`RESTORE_CHECK_HOOK` receives the temporary `PGDATABASE` for isolated application
smoke checks. Run with email/provider egress disabled and no public listener. A
failed restore leaves the original database untouched. The automated fixture drill
also compares every row in every app/auth table and verifies offline public reads,
private export, startup migrations and receipt recovery using restored sessions.
The PostgreSQL [dump](https://www.postgresql.org/docs/17/backup-dump.html) and
[restore](https://www.postgresql.org/docs/17/app-pgrestore.html) manuals document the
archive and restore semantics.

Disaster recovery must stay isolated until the owner has reconciled deletions,
publication revocations, blocks and moderation decisions since the snapshot. A
backup can otherwise resurrect deleted accounts/content or old public permissions.
Do not expose the restored public API merely because the dump loaded. Revoke old
sessions as part of controlled recovery, scrub expired evidence/receipts/audits,
validate privacy with the restored state, and obtain an operational cutover decision.
If post-snapshot deletion/revocation records cannot be reconstructed, keep public
serving disabled and resolve the loss explicitly; never assume an old public flag
is current consent. Record RPO loss and actual restore/cutover times.

## Browser and deployment acceptance

Build the disposable harness with `scripts/build-acceptance.sh NEW_OUTPUT_DIRECTORY`.
Serve that directory at `/acceptance/` on the **same HTTPS origin and RP ID** as the
candidate deployment, with API/auth requests routed through the real edge. The
harness uses the checked-in TypeScript authentication adapter, cookie sessions and
browser credential API. It has no auth bypass, storage of tokens or fake passkeys.
Remove it after acceptance. Use controlled test accounts and an operator-owned
mailbox. When a real email lands on `/signup`, `/recovery` or `/account/email`, paste
its token into the harness without sharing/logging it (or temporarily serve the
harness at those paths). Secure links must not leak into URL-referrer logs.

Record hostname, origin/RP ID, server/proxy versions, exact redacted configuration,
build identity, browser/OS/authenticator, UTC date, result and evidence location:

1. From two independent external clients, show separate rate-limit buckets and
   persisted audit IPs; spoofed XFF/Forwarded/X-Real-IP must not change identity.
   Show malformed forwarding fails closed, backend port is unreachable publicly,
   and nginx configuration validates. Test IPv4 and IPv6 where deployed.
2. Receive production signup email through the real SMTP server; verify sender,
   SPF/DKIM/DMARC results, link origin and successful delivery. Complete signup
   using a real platform/security-key authenticator. Check `__Host-` cookie with
   Secure/HttpOnly/Path=/ and intended SameSite. Verify no token in localStorage.
3. Sign out, sign in with the same passkey, test a second browser/device, cancel a
   ceremony, and reject wrong-origin requests. Add/remove a second passkey; removal
   requires step-up. Revoke another session and verify the other browser loses
   access immediately. Test logout-everywhere with a previously warm session.
4. Receive recovery email, complete with a new passkey, verify old-link reuse fails
   and destructive actions return the recovery cooldown. Do not weaken the real
   24-hour cooldown for acceptance. After it expires, re-test sensitive actions.
5. Request email change using step-up, receive/confirm the real email, test token
   reuse, and delete the disposable account with step-up. Verify public profile,
   direct items, feed, lists and summaries disappear for anonymous and signed-in
   observers; cached/repeated GET/HEAD must not return old content or validators.
6. Exercise independent visibility using owner, other account and anonymous reads,
   including profile-disabled visitor preview, block and moderation restrictions.
   Edit/backdate old reviews and notes, verify chosen chronology and absent private
   timestamps/revisions; test parent/item/account deletion and idempotent retry.
7. Block provider egress: stored library/diary/public data still works; unstored
   imports fail atomically. Restore provider access and retry safely. Verify backup
   offsite download, checksum, isolated restore, privacy checks and timed recovery.

Run `scripts/verify.sh --database` for the automated privacy/concurrency/provider
matrix and `scripts/verify.sh --restore` with PG17 clients for the complete fixture
restore drill. Do not label real WebAuthn, production email, edge/CDN cache behavior,
offsite backup recovery, or deployment-specific topology passed from these tests.
At the owner's request, unperformed environment checks are explicit release
blockers in roadmap item 14. Item 13 covers engineering and local verification;
its completion is not evidence that the actual deployment has been accepted.
