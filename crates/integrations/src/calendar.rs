//! The meeting's calendar event: its subject names the meeting and its attendees become
//! speaker candidates.
//!
//! Two sources:
//!
//! - **Microsoft Graph** (`/me/calendarView`), signed in with the OAuth 2.0 device code
//!   flow as a public client: the user's own Entra ID app registration, no client secret
//!   and no local redirect server. [`start_device_code`] gets a code to show, the app
//!   calls [`poll_device_code`] every `interval` seconds until it yields tokens, and
//!   [`refresh`] renews them.
//! - **ICS** — a published calendar URL ([`fetch_ics`]) or file, parsed by
//!   [`parse_ics`], for tenants where Graph needs an admin's consent.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone as _, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{self, Reply, enc, first_line, form};
use crate::{Error, Result};

/// How far ahead of an event's start [`event_at`] still picks it (seconds).
const EARLY_WINDOW_SECS: i64 = 10 * 60;

/// The delegated permissions asked for.
pub const GRAPH_SCOPES: &str = "offline_access Calendars.Read User.Read";

/// Pages of `calendarView` followed at most.
const MAX_PAGES: usize = 10;

/// Someone invited.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Attendee {
    /// Display name (the email when the calendar gives no name).
    pub name: String,
    /// Email address.
    pub email: Option<String>,
}

/// A calendar event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Its subject, the meeting's title.
    pub subject: String,
    /// Start, Unix seconds.
    pub start: i64,
    /// End, Unix seconds.
    pub end: i64,
    /// Who was invited (rooms and resources left out).
    pub attendees: Vec<Attendee>,
    /// The organizer's name (or email).
    pub organizer: Option<String>,
    /// It has a Teams meeting link.
    pub online_meeting: bool,
}

/// The event that best matches a meeting starting at `at` (Unix seconds): one in
/// progress at `at`, or starting at most 10 minutes after it. Online (Teams) meetings
/// win over others; then the one starting closest to `at`.
pub fn event_at(events: &[CalendarEvent], at: i64) -> Option<&CalendarEvent> {
    events
        .iter()
        .filter(|e| {
            let in_progress = e.start <= at && at < e.end.max(e.start + 1);
            let soon = e.start > at && e.start - at <= EARLY_WINDOW_SECS;
            in_progress || soon
        })
        .min_by_key(|e| (!e.online_meeting, (e.start - at).abs()))
}

// ---------------------------------------------------------------------------------------
// Microsoft Graph

/// The app registration to sign in with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphAuth {
    /// The application (client) id of the user's own Entra ID app registration, set up
    /// as a public client ("Allow public client flows") with delegated `Calendars.Read`
    /// and `User.Read`.
    pub client_id: String,
    /// "organizations" (work accounts, the default), "common", or a tenant id.
    pub tenant: String,
}

impl GraphAuth {
    /// An app registration in the "organizations" tenant.
    pub fn new(client_id: impl Into<String>) -> GraphAuth {
        GraphAuth {
            client_id: client_id.into(),
            tenant: "organizations".to_owned(),
        }
    }

    fn tenant(&self) -> &str {
        let t = self.tenant.trim();
        if t.is_empty() { "organizations" } else { t }
    }
}

/// A device code to show the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCode {
    /// What the user types at [`DeviceCode::verification_uri`].
    pub user_code: String,
    /// Where they type it (`https://microsoft.com/devicelogin`).
    pub verification_uri: String,
    /// Microsoft's own instruction text.
    #[serde(default)]
    pub message: String,
    /// The code the app polls with (not shown).
    pub device_code: String,
    /// Seconds to wait between polls.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Seconds until the code expires.
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

/// Signed-in tokens. Keep them sealed (the store's secrets), never in settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Bearer token for Graph calls.
    pub access_token: String,
    /// Token to get new access tokens with ([`refresh`]).
    pub refresh_token: String,
    /// When the access token expires, Unix seconds.
    pub expires_at: i64,
    /// The signed-in account ("Jeremie Lumandong" or the email), when known.
    pub account: Option<String>,
}

impl Tokens {
    /// The access token has expired at `now` (or will within a minute).
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at - 60 <= now
    }
}

/// One step of polling for the device code sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    /// The user has not finished signing in; poll again after the interval.
    Pending,
    /// Polling too fast; add 5 seconds to the interval.
    SlowDown,
    /// Signed in.
    Token(Tokens),
    /// The user declined.
    Declined,
    /// The code expired; start again.
    Expired,
}

/// `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode`.
pub fn device_code_url(auth: &GraphAuth) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
        enc(auth.tenant())
    )
}

/// `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`.
pub fn token_url(auth: &GraphAuth) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        enc(auth.tenant())
    )
}

/// The device code request's form body.
pub fn device_code_form(auth: &GraphAuth) -> String {
    form(&[
        ("client_id", auth.client_id.trim()),
        ("scope", GRAPH_SCOPES),
    ])
}

/// One poll's form body.
pub fn poll_form(auth: &GraphAuth, code: &DeviceCode) -> String {
    form(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("client_id", auth.client_id.trim()),
        ("device_code", &code.device_code),
    ])
}

/// The refresh request's form body.
pub fn refresh_form(auth: &GraphAuth, refresh_token: &str) -> String {
    form(&[
        ("grant_type", "refresh_token"),
        ("client_id", auth.client_id.trim()),
        ("refresh_token", refresh_token),
        ("scope", GRAPH_SCOPES),
    ])
}

/// `calendarView` between two Unix times, UTC, with only the fields used.
pub fn calendar_view_url(from: i64, to: i64) -> String {
    let iso = |t: i64| {
        DateTime::<Utc>::from_timestamp(t, 0)
            .unwrap_or_default()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    };
    format!(
        "https://graph.microsoft.com/v1.0/me/calendarView?startDateTime={}&endDateTime={}\
         &$select=subject,start,end,attendees,organizer,isOnlineMeeting,onlineMeeting,\
         isAllDay,isCancelled&$top=50",
        enc(&iso(from)),
        enc(&iso(to))
    )
}

fn post_form(url: &str, body: String) -> Result<Reply> {
    http::finish(
        "Microsoft",
        http::agent()
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .send(body),
    )
}

fn bad(detail: impl Into<String>) -> Error {
    Error::BadResponse {
        service: "Microsoft",
        detail: detail.into(),
    }
}

/// The `error` and first line of `error_description` of an OAuth error reply.
fn oauth_error(body: &str) -> (String, String) {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    (
        v["error"].as_str().unwrap_or_default().to_owned(),
        first_line(v["error_description"].as_str().unwrap_or(body)),
    )
}

/// A failed sign-in reply as a readable error.
fn sign_in_error(reply: &Reply) -> Error {
    let (code, desc) = oauth_error(&reply.body);
    match code.as_str() {
        "invalid_grant" | "interaction_required" => {
            Error::Auth(format!("Microsoft sign-in expired; sign in again ({desc})"))
        }
        "invalid_client" | "unauthorized_client" => Error::Rejected(format!(
            "Microsoft does not accept this app registration; check the client id and that \
             public client flows are allowed ({desc})"
        )),
        "" => Error::Rejected(format!("Microsoft sign-in failed ({})", reply.status)),
        _ => Error::Rejected(format!("Microsoft sign-in failed: {desc}")),
    }
}

/// Start signing in: a code for the user to enter at `verification_uri`.
pub fn start_device_code(auth: &GraphAuth) -> Result<DeviceCode> {
    let reply = post_form(&device_code_url(auth), device_code_form(auth))?;
    if !reply.ok() {
        return Err(sign_in_error(&reply));
    }
    serde_json::from_str(&reply.body).map_err(|e| bad(e.to_string()))
}

/// The tokens in a token reply; `previous_refresh` is kept when none is returned.
fn parse_tokens(body: &str, now: i64, previous_refresh: Option<&str>) -> Result<Tokens> {
    let v: Value = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
    let access_token = v["access_token"]
        .as_str()
        .ok_or_else(|| bad("no access_token"))?
        .to_owned();
    let refresh_token = v["refresh_token"]
        .as_str()
        .or(previous_refresh)
        .unwrap_or_default()
        .to_owned();
    let expires_in = v["expires_in"]
        .as_i64()
        .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(3600);
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at: now + expires_in,
        account: None,
    })
}

/// What a poll reply means.
fn parse_poll(reply: &Reply, now: i64) -> Result<Poll> {
    if reply.ok() {
        return parse_tokens(&reply.body, now, None).map(Poll::Token);
    }
    let (code, _) = oauth_error(&reply.body);
    Ok(match code.as_str() {
        "authorization_pending" => Poll::Pending,
        "slow_down" => Poll::SlowDown,
        "authorization_declined" | "access_denied" => Poll::Declined,
        "expired_token" | "code_expired" => Poll::Expired,
        _ => return Err(sign_in_error(reply)),
    })
}

fn now() -> i64 {
    Utc::now().timestamp()
}

/// One poll of the device code sign-in. On success the account name is looked up
/// (`/me`); a failed lookup leaves it `None`.
pub fn poll_device_code(auth: &GraphAuth, code: &DeviceCode) -> Result<Poll> {
    let reply = post_form(&token_url(auth), poll_form(auth, code))?;
    let poll = parse_poll(&reply, now())?;
    if let Poll::Token(mut t) = poll {
        t.account = me(&t.access_token)
            .map_err(|e| tracing::warn!("could not read the Graph account: {e}"))
            .ok();
        tracing::info!("signed in to Microsoft Graph");
        return Ok(Poll::Token(t));
    }
    Ok(poll)
}

/// New tokens from a refresh token (the account name is not filled in; keep the old one).
pub fn refresh(auth: &GraphAuth, refresh_token: &str) -> Result<Tokens> {
    let reply = post_form(&token_url(auth), refresh_form(auth, refresh_token))?;
    if !reply.ok() {
        return Err(sign_in_error(&reply));
    }
    parse_tokens(&reply.body, now(), Some(refresh_token))
}

fn graph_get(access_token: &str, url: &str) -> Result<Value> {
    let reply = http::finish(
        "Microsoft Graph",
        http::agent()
            .get(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/json")
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .call(),
    )?;
    if !reply.ok() {
        let v: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
        let msg = first_line(v["error"]["message"].as_str().unwrap_or_default());
        return Err(match reply.status {
            401 => Error::Auth(format!("Microsoft sign-in expired ({msg})")),
            403 => Error::Rejected(format!(
                "Your organization has not allowed reading your calendar (an admin may need \
                 to consent); use a published ICS calendar instead ({msg})"
            )),
            429 => Error::Rejected("Microsoft Graph is limiting requests; try later".to_owned()),
            s => Error::Rejected(format!("Microsoft Graph answered {s} ({msg})")),
        });
    }
    serde_json::from_str(&reply.body).map_err(|e| bad(e.to_string()))
}

/// The signed-in user's display name (or email).
pub fn me(access_token: &str) -> Result<String> {
    let v = graph_get(
        access_token,
        "https://graph.microsoft.com/v1.0/me?$select=displayName,mail,userPrincipalName",
    )?;
    ["displayName", "mail", "userPrincipalName"]
        .iter()
        .find_map(|k| v[k].as_str().filter(|s| !s.is_empty()))
        .map(str::to_owned)
        .ok_or_else(|| bad("no displayName"))
}

/// Events between `from` and `to` (Unix seconds), recurring ones expanded by Graph;
/// cancelled and all-day events left out.
pub fn calendar_view(access_token: &str, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
    let mut url = calendar_view_url(from, to);
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = graph_get(access_token, &url)?;
        out.extend(parse_graph_events(&page)?);
        match page["@odata.nextLink"].as_str() {
            Some(next) => url = next.to_owned(),
            None => break,
        }
    }
    Ok(out)
}

/// A Graph `dateTimeTimeZone` (asked for in UTC) as Unix seconds.
fn graph_time(v: &Value) -> Option<i64> {
    let s = v["dateTime"].as_str()?;
    let s = s.trim_end_matches('Z');
    let ndt = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    Some(ndt.and_utc().timestamp())
}

fn graph_person(v: &Value) -> Option<Attendee> {
    let email = v["emailAddress"]["address"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let name = v["emailAddress"]["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
        .or_else(|| email.clone())?;
    Some(Attendee { name, email })
}

/// The events in one `calendarView` page.
pub fn parse_graph_events(page: &Value) -> Result<Vec<CalendarEvent>> {
    let items = page["value"]
        .as_array()
        .ok_or_else(|| bad("calendarView has no value list"))?;
    let mut out = Vec::new();
    for e in items {
        if e["isCancelled"].as_bool() == Some(true) || e["isAllDay"].as_bool() == Some(true) {
            continue;
        }
        let (Some(start), Some(end)) = (graph_time(&e["start"]), graph_time(&e["end"])) else {
            tracing::debug!("skipping a calendar event without times");
            continue;
        };
        let attendees = e["attendees"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|p| p["type"].as_str() != Some("resource"))
                    .filter_map(graph_person)
                    .collect()
            })
            .unwrap_or_default();
        let join_url = e["onlineMeeting"]["joinUrl"]
            .as_str()
            .is_some_and(|u| !u.is_empty());
        out.push(CalendarEvent {
            subject: e["subject"].as_str().unwrap_or_default().trim().to_owned(),
            start,
            end,
            attendees,
            organizer: graph_person(&e["organizer"]).map(|p| p.name),
            online_meeting: e["isOnlineMeeting"].as_bool() == Some(true) || join_url,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// ICS

/// `webcal://` (as Outlook publishes calendars) as `https://`.
pub fn ics_http_url(url: &str) -> String {
    let url = url.trim();
    match url.get(..9) {
        Some(p) if p.eq_ignore_ascii_case("webcal://") => format!("https://{}", &url[9..]),
        _ => url.to_owned(),
    }
}

/// Download a published calendar (an Outlook "Publish calendar" ICS link).
pub fn fetch_ics(url: &str) -> Result<String> {
    let url = ics_http_url(url);
    let reply = http::finish(
        "the calendar URL",
        http::agent()
            .get(&url)
            .header("Accept", "text/calendar, */*")
            .call(),
    )?;
    if !reply.ok() {
        return Err(Error::Rejected(format!(
            "the calendar URL answered {} (is the calendar still published?)",
            reply.status
        )));
    }
    if !reply.body.contains("BEGIN:VCALENDAR") {
        return Err(Error::BadResponse {
            service: "the calendar URL",
            detail: "not an iCalendar file".to_owned(),
        });
    }
    Ok(reply.body)
}

/// One content line: name, parameters and value.
struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Line {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Split `NAME;P=V;Q="a:b":value` at the first `:` outside quotes.
fn parse_line(line: &str) -> Option<Line> {
    let mut in_quotes = false;
    let mut split = None;
    let mut cuts = Vec::new();
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => cuts.push(i),
            ':' if !in_quotes => {
                split = Some(i);
                break;
            }
            _ => {}
        }
    }
    let split = split?;
    let head = &line[..split];
    let mut bounds = vec![0];
    bounds.extend(cuts.iter().map(|c| c + 1));
    let mut parts = Vec::new();
    for (n, start) in bounds.iter().enumerate() {
        let end = cuts.get(n).copied().unwrap_or(head.len());
        parts.push(&head[*start..end]);
    }
    let name = parts.first()?.trim().to_ascii_uppercase();
    let params = parts[1..]
        .iter()
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect();
    Some(Line {
        name,
        params,
        value: line[split + 1..].to_owned(),
    })
}

/// Undo iCalendar text escaping (`\,` `\;` `\n` `\\`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A DATE-TIME as Unix seconds: UTC with `Z`, otherwise (a `TZID` or floating time) the
/// machine's local time. A DATE (all day) is `None`.
fn ics_time(line: &Line) -> Option<i64> {
    let v = line.value.trim();
    if line
        .param("VALUE")
        .is_some_and(|t| t.eq_ignore_ascii_case("DATE"))
        || !v.contains('T')
    {
        return None;
    }
    if let Some(utc) = v.strip_suffix('Z').or_else(|| v.strip_suffix('z')) {
        let ndt = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(ndt.and_utc().timestamp());
    }
    let ndt = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    Local
        .from_local_datetime(&ndt)
        .earliest()
        .map(|t| t.timestamp())
}

/// A simple `DURATION` (`PT1H30M`, `P1D`, `-PT5M` is refused) in seconds.
fn ics_duration(v: &str) -> Option<i64> {
    let v = v.trim().strip_prefix('+').unwrap_or(v.trim());
    let rest = v.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            unit => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match (unit, in_time) {
                    ('W', false) => 7 * 86_400,
                    ('D', false) => 86_400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    num.is_empty().then_some(total)
}

/// `mailto:x@y` → `x@y`.
fn mailto(v: &str) -> Option<String> {
    let v = v.trim();
    let addr = match v.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => &v[7..],
        _ => v,
    };
    (!addr.is_empty() && addr.contains('@')).then(|| addr.to_owned())
}

fn ics_person(line: &Line) -> Option<Attendee> {
    let email = mailto(&line.value);
    let name = line
        .param("CN")
        .map(|n| unescape(n).trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| email.clone())?;
    Some(Attendee { name, email })
}

fn mentions_teams(s: &str) -> bool {
    s.to_ascii_lowercase().contains("teams.microsoft.com")
}

/// Events in an iCalendar file.
///
/// Folded lines are unfolded and text unescaped. Times in UTC (`Z`) are exact; times with
/// a `TZID` (and floating times) are read as this machine's local time, which is right
/// for the user's own calendar but not for an event created in another time zone.
/// All-day and cancelled events are skipped, as are rooms and resources among attendees.
/// An event is online when it has `X-MICROSOFT-SKYPETEAMSMEETINGURL` or a
/// `teams.microsoft.com` link in its description, location or URL.
///
/// Recurrence (`RRULE`, `RDATE`, `EXDATE`) is not expanded: a recurring event appears
/// once, at its first instance, and instances moved with `RECURRENCE-ID` appear as their
/// own events. Published Outlook calendars list a moved or upcoming occurrence that
/// way, but a plain weekly series only matches on its first day; Graph has no such
/// limit.
pub fn parse_ics(text: &str) -> Vec<CalendarEvent> {
    // Unfold: a line starting with a space or tab continues the previous one.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match raw.strip_prefix([' ', '\t']) {
            Some(cont) if !lines.is_empty() => {
                if let Some(last) = lines.last_mut() {
                    last.push_str(cont);
                }
            }
            _ => lines.push(raw.to_owned()),
        }
    }

    #[derive(Default)]
    struct Draft {
        subject: String,
        start: Option<i64>,
        end: Option<i64>,
        duration: Option<i64>,
        all_day: bool,
        cancelled: bool,
        attendees: Vec<Attendee>,
        organizer: Option<String>,
        online: bool,
    }

    let mut out = Vec::new();
    let mut event: Option<Draft> = None;
    // Components nested inside the event (VALARM) whose properties are not the event's.
    let mut nested = 0usize;
    for raw in &lines {
        let Some(line) = parse_line(raw) else {
            continue;
        };
        let value_upper = line.value.trim().to_ascii_uppercase();
        match (line.name.as_str(), event.as_mut()) {
            ("BEGIN", None) if value_upper == "VEVENT" => {
                event = Some(Draft::default());
                nested = 0;
            }
            ("BEGIN", Some(_)) => nested += 1,
            ("END", Some(_)) if nested > 0 => nested -= 1,
            ("END", Some(_)) if value_upper == "VEVENT" => {
                let Some(d) = event.take() else { continue };
                if d.all_day || d.cancelled {
                    continue;
                }
                let Some(start) = d.start else { continue };
                let end = d.end.or(d.duration.map(|x| start + x)).unwrap_or(start);
                out.push(CalendarEvent {
                    subject: d.subject,
                    start,
                    end: end.max(start),
                    attendees: d.attendees,
                    organizer: d.organizer,
                    online_meeting: d.online,
                });
            }
            (_, Some(_)) if nested > 0 => {}
            (name, Some(d)) => match name {
                "SUMMARY" => d.subject = unescape(&line.value).trim().to_owned(),
                "DTSTART" => {
                    d.start = ics_time(&line);
                    d.all_day = d.start.is_none();
                }
                "DTEND" => d.end = ics_time(&line),
                "DURATION" => d.duration = ics_duration(&line.value),
                "STATUS" => d.cancelled = value_upper == "CANCELLED",
                "ATTENDEE" => {
                    let kind = line.param("CUTYPE").unwrap_or("INDIVIDUAL");
                    let person = kind.eq_ignore_ascii_case("INDIVIDUAL")
                        || kind.eq_ignore_ascii_case("UNKNOWN");
                    if person
                        && let Some(a) = ics_person(&line)
                        && !d.attendees.contains(&a)
                    {
                        d.attendees.push(a);
                    }
                }
                "ORGANIZER" => d.organizer = ics_person(&line).map(|p| p.name),
                "X-MICROSOFT-SKYPETEAMSMEETINGURL" => d.online = true,
                "DESCRIPTION" | "LOCATION" | "URL" | "X-ALT-DESC" => {
                    d.online |= mentions_teams(&line.value);
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// The local calendar day of a Unix time (for asking a calendar about "today").
pub fn local_day(at: i64) -> NaiveDate {
    DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(subject: &str, start: i64, end: i64, online: bool) -> CalendarEvent {
        CalendarEvent {
            subject: subject.into(),
            start,
            end,
            attendees: vec![],
            organizer: None,
            online_meeting: online,
        }
    }

    #[test]
    fn picks_the_covering_event() {
        let t = 1_000_000;
        let events = vec![
            ev("Long block", t - 3600, t + 3600, false),
            ev("Standup", t + 300, t + 1200, true),
            ev("Later", t + 1200, t + 2400, true),
            ev("Over", t - 7200, t - 3600, true),
        ];
        // The online meeting starting in 5 minutes beats the offline block in progress.
        assert_eq!(event_at(&events, t).unwrap().subject, "Standup");
        // Among offline only, the in-progress one.
        assert_eq!(event_at(&events[..1], t).unwrap().subject, "Long block");
        // Joined late: still in progress.
        assert_eq!(event_at(&events[..2], t + 900).unwrap().subject, "Standup");
        // With the next meeting 5 minutes away, the closer start wins.
        assert_eq!(event_at(&events, t + 900).unwrap().subject, "Later");
        // Nothing within 10 minutes.
        assert!(event_at(&events[2..], t).is_none());
        assert!(event_at(&[], t).is_none());
        // Two online candidates: the closest start.
        let two = vec![
            ev("A", t - 600, t + 600, true),
            ev("B", t + 120, t + 900, true),
        ];
        assert_eq!(event_at(&two, t).unwrap().subject, "B");
    }

    #[test]
    fn graph_requests() {
        let auth = GraphAuth::new("abc-123");
        assert_eq!(
            device_code_url(&auth),
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/devicecode"
        );
        assert_eq!(
            device_code_form(&auth),
            "client_id=abc-123&scope=offline_access%20Calendars.Read%20User.Read"
        );
        let code = DeviceCode {
            user_code: "ABCD".into(),
            verification_uri: "https://microsoft.com/devicelogin".into(),
            message: String::new(),
            device_code: "dc+/=".into(),
            interval: 5,
            expires_in: 900,
        };
        assert_eq!(
            poll_form(&auth, &code),
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code\
             &client_id=abc-123&device_code=dc%2B%2F%3D"
        );
        assert!(refresh_form(&auth, "r1").starts_with("grant_type=refresh_token&"));
        let url = calendar_view_url(1_792_051_200, 1_792_137_600);
        assert!(url.starts_with(
            "https://graph.microsoft.com/v1.0/me/calendarView?\
             startDateTime=2026-10-15T08%3A00%3A00Z&endDateTime=2026-10-16T08%3A00%3A00Z&$select="
        ));
        assert!(url.contains("isOnlineMeeting,onlineMeeting"));
        assert!(url.ends_with("&$top=50"));
    }

    #[test]
    fn device_code_and_polls() {
        let dc: DeviceCode = serde_json::from_str(
            r#"{"user_code":"F2K9","device_code":"long","verification_uri":
               "https://microsoft.com/devicelogin","expires_in":900,"interval":5,
               "message":"To sign in, use a web browser…"}"#,
        )
        .unwrap();
        assert_eq!(dc.user_code, "F2K9");
        let r = |status, body: &str| Reply {
            status,
            body: body.into(),
        };
        let p = |status, body: &str| parse_poll(&r(status, body), 100);
        assert_eq!(
            p(400, r#"{"error":"authorization_pending"}"#).unwrap(),
            Poll::Pending
        );
        assert_eq!(p(400, r#"{"error":"slow_down"}"#).unwrap(), Poll::SlowDown);
        assert_eq!(
            p(400, r#"{"error":"authorization_declined"}"#).unwrap(),
            Poll::Declined
        );
        assert_eq!(
            p(400, r#"{"error":"expired_token"}"#).unwrap(),
            Poll::Expired
        );
        let err = p(
            400,
            r#"{"error":"invalid_client","error_description":"AADSTS7000218: no secret\r\nTrace"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("AADSTS7000218: no secret"));
        assert!(!err.to_string().contains("Trace"));
        let ok = p(
            200,
            r#"{"token_type":"Bearer","access_token":"at","refresh_token":"rt","expires_in":3599}"#,
        )
        .unwrap();
        let Poll::Token(t) = ok else { panic!("{ok:?}") };
        assert_eq!(
            (t.access_token.as_str(), t.refresh_token.as_str()),
            ("at", "rt")
        );
        assert_eq!(t.expires_at, 3699);
        assert!(!t.expired(3000));
        assert!(t.expired(3650));
        // A refresh reply without a new refresh token keeps the old one.
        let t = parse_tokens(r#"{"access_token":"a2","expires_in":"60"}"#, 0, Some("rt")).unwrap();
        assert_eq!((t.refresh_token.as_str(), t.expires_at), ("rt", 60));
        let e = sign_in_error(&r(
            400,
            r#"{"error":"invalid_grant","error_description":"x"}"#,
        ));
        assert!(matches!(e, Error::Auth(_)));
    }

    const GRAPH_PAGE: &str = r#"{
      "@odata.context": "…",
      "value": [
        {
          "subject": "AKS cutover sync",
          "isAllDay": false, "isCancelled": false,
          "start": {"dateTime": "2026-10-15T08:00:00.0000000", "timeZone": "UTC"},
          "end": {"dateTime": "2026-10-15T08:45:00.0000000", "timeZone": "UTC"},
          "isOnlineMeeting": false,
          "onlineMeeting": {"joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3a"},
          "organizer": {"emailAddress": {"name": "Priya Nair", "address": "priya@contoso.com"}},
          "attendees": [
            {"type": "required", "emailAddress": {"name": "Jeremie Lumandong", "address": "jeremie@contoso.com"}},
            {"type": "optional", "emailAddress": {"name": "", "address": "dana@contoso.com"}},
            {"type": "resource", "emailAddress": {"name": "Room 4", "address": "room4@contoso.com"}}
          ]
        },
        {
          "subject": "Holiday", "isAllDay": true,
          "start": {"dateTime": "2026-10-15T00:00:00.0000000", "timeZone": "UTC"},
          "end": {"dateTime": "2026-10-16T00:00:00.0000000", "timeZone": "UTC"}
        },
        {
          "subject": "Cancelled", "isCancelled": true,
          "start": {"dateTime": "2026-10-15T09:00:00.0000000", "timeZone": "UTC"},
          "end": {"dateTime": "2026-10-15T09:30:00.0000000", "timeZone": "UTC"}
        },
        {
          "subject": "Focus", "isOnlineMeeting": false, "onlineMeeting": null,
          "start": {"dateTime": "2026-10-15T10:00:00", "timeZone": "UTC"},
          "end": {"dateTime": "2026-10-15T11:00:00", "timeZone": "UTC"},
          "organizer": {"emailAddress": {"address": "me@contoso.com"}}
        }
      ]
    }"#;

    #[test]
    fn parses_graph_events() {
        let page: Value = serde_json::from_str(GRAPH_PAGE).unwrap();
        let events = parse_graph_events(&page).unwrap();
        assert_eq!(events.len(), 2);
        let e = &events[0];
        assert_eq!(e.subject, "AKS cutover sync");
        assert_eq!(e.start, 1_792_051_200);
        assert_eq!(e.end, 1_792_051_200 + 45 * 60);
        assert!(e.online_meeting);
        assert_eq!(e.organizer.as_deref(), Some("Priya Nair"));
        assert_eq!(
            e.attendees,
            [
                Attendee {
                    name: "Jeremie Lumandong".into(),
                    email: Some("jeremie@contoso.com".into())
                },
                Attendee {
                    name: "dana@contoso.com".into(),
                    email: Some("dana@contoso.com".into())
                },
            ]
        );
        assert!(!events[1].online_meeting);
        assert_eq!(events[1].organizer.as_deref(), Some("me@contoso.com"));
        assert!(parse_graph_events(&serde_json::json!({})).is_err());
        // Round-trips through the JSON the app stores with the meeting.
        let json = serde_json::to_string(e).unwrap();
        assert_eq!(&serde_json::from_str::<CalendarEvent>(&json).unwrap(), e);
    }

    const ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:Microsoft Exchange Server 2010\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:W. Europe Standard Time\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
SUMMARY;LANGUAGE=en-US:AKS cutover\\, phase 2\\; GPU pools\r\n\
DTSTART:20261015T080000Z\r\n\
DTEND:20261015T084500Z\r\n\
ORGANIZER;CN=\"Nair, Priya\":mailto:priya@contoso.com\r\n\
ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Jeremie Lumandong;RSVP=TRUE:mai\r\n\
\x20lto:jeremie@contoso.com\r\n\
ATTENDEE;CUTYPE=ROOM;CN=Room 4:mailto:room4@contoso.com\r\n\
ATTENDEE;RSVP=TRUE:mailto:dana@contoso.com\r\n\
DESCRIPTION:Agenda:\\n1. Pools\\n\\nJoin: https://teams.micro\r\n\
\tsoft.com/l/meetup-join/abc\r\n\
BEGIN:VALARM\r\n\
DESCRIPTION:REMINDER\r\n\
TRIGGER:-PT15M\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Weekly sync\r\n\
RRULE:FREQ=WEEKLY;BYDAY=TH\r\n\
DTSTART;TZID=W. Europe Standard Time:20261015T140000\r\n\
DURATION:PT30M\r\n\
X-MICROSOFT-SKYPETEAMSMEETINGURL:https://teams.microsoft.com/l/x\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Day off\r\n\
DTSTART;VALUE=DATE:20261016\r\n\
DTEND;VALUE=DATE:20261017\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:Cancelled\r\n\
STATUS:CANCELLED\r\n\
DTSTART:20261015T090000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_ics() {
        let events = parse_ics(ICS);
        assert_eq!(events.len(), 2, "{events:#?}");
        let e = &events[0];
        assert_eq!(e.subject, "AKS cutover, phase 2; GPU pools");
        assert_eq!((e.start, e.end), (1_792_051_200, 1_792_051_200 + 2700));
        assert_eq!(e.organizer.as_deref(), Some("Nair, Priya"));
        assert_eq!(
            e.attendees,
            [
                Attendee {
                    name: "Jeremie Lumandong".into(),
                    email: Some("jeremie@contoso.com".into())
                },
                Attendee {
                    name: "dana@contoso.com".into(),
                    email: Some("dana@contoso.com".into())
                },
            ]
        );
        assert!(e.online_meeting, "Teams link in a folded DESCRIPTION");

        // TZID times are read as local time.
        let w = &events[1];
        let local = Local
            .with_ymd_and_hms(2026, 10, 15, 14, 0, 0)
            .earliest()
            .unwrap()
            .timestamp();
        assert_eq!(w.subject, "Weekly sync");
        assert_eq!((w.start, w.end), (local, local + 1800));
        assert!(w.online_meeting);
        assert!(w.attendees.is_empty());
    }

    #[test]
    fn ics_pieces() {
        assert_eq!(unescape(r"a\, b\; c\nd\\e\N"), "a, b; c\nd\\e\n");
        assert_eq!(ics_duration("PT1H30M"), Some(5400));
        assert_eq!(ics_duration("P1DT1S"), Some(86_401));
        assert_eq!(ics_duration("P1W"), Some(604_800));
        assert_eq!(ics_duration("-PT5M"), None);
        assert_eq!(ics_duration("PT5"), None);
        assert_eq!(
            ics_http_url("webcal://outlook.office365.com/x.ics"),
            "https://outlook.office365.com/x.ics"
        );
        assert_eq!(ics_http_url(" https://a/b.ics "), "https://a/b.ics");
        let l = parse_line("ATTENDEE;CN=\"A;B: C\";X=1:MAILTO:a@b.c").unwrap();
        assert_eq!(l.name, "ATTENDEE");
        assert_eq!(l.param("cn"), Some("A;B: C"));
        assert_eq!(l.param("X"), Some("1"));
        assert_eq!(mailto(&l.value).as_deref(), Some("a@b.c"));
        assert!(parse_line("no colon").is_none());
        // LF-only files and a stray leading fold parse too.
        let lf = ICS.replace("\r\n", "\n");
        assert_eq!(parse_ics(&lf), parse_ics(ICS));
        assert!(parse_ics(" junk\nBEGIN:VEVENT\n").is_empty());
    }
}
