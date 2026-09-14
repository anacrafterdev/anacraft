//! Thin client over the GA4 Data API (`runReport`, `runRealtimeReport`) and
//! the Admin API (`accountSummaries`). One endpoint family, so a hand-rolled
//! client beats pulling in a generated SDK.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth::Auth;

const DATA_API: &str = "https://analyticsdata.googleapis.com/v1beta";
const ADMIN_API: &str = "https://analyticsadmin.googleapis.com/v1beta";

// ---------------------------------------------------------------- request ---

#[derive(Serialize)]
pub struct DateRange {
    #[serde(rename = "startDate")]
    pub start_date: String,
    #[serde(rename = "endDate")]
    pub end_date: String,
}

impl DateRange {
    /// GA4 accepts relative dates; `NdaysAgo` keeps us off local-clock math.
    /// `yesterday` is the end because today's data is still partial.
    pub fn last_days(days: u32) -> DateRange {
        DateRange {
            start_date: format!("{}daysAgo", days),
            end_date: "yesterday".to_string(),
        }
    }

    /// The equivalent window immediately before `last_days`, for deltas.
    pub fn previous_days(days: u32) -> DateRange {
        DateRange {
            start_date: format!("{}daysAgo", days * 2),
            end_date: format!("{}daysAgo", days + 1),
        }
    }

    /// The single most recent complete day.
    pub fn yesterday() -> DateRange {
        DateRange {
            start_date: "yesterday".to_string(),
            end_date: "yesterday".to_string(),
        }
    }

    /// An arbitrary `NdaysAgo` span for the windows the named constructors do
    /// not cover. `end` is the more recent bound, so `span(29, 2)` is the 28
    /// days ending the day before yesterday.
    pub fn span(start_days_ago: u32, end_days_ago: u32) -> DateRange {
        DateRange {
            start_date: format!("{}daysAgo", start_days_ago),
            end_date: format!("{}daysAgo", end_days_ago),
        }
    }
}

#[derive(Serialize)]
pub struct Named {
    pub name: String,
}

impl Named {
    pub fn list(names: &[&str]) -> Vec<Named> {
        names
            .iter()
            .map(|n| Named {
                name: n.to_string(),
            })
            .collect()
    }
}

#[derive(Serialize)]
pub struct MetricOrderBy {
    #[serde(rename = "metricName")]
    pub metric_name: String,
}

#[derive(Serialize)]
pub struct OrderBy {
    pub metric: MetricOrderBy,
    pub desc: bool,
}

impl OrderBy {
    pub fn desc(metric: &str) -> OrderBy {
        OrderBy {
            metric: MetricOrderBy {
                metric_name: metric.to_string(),
            },
            desc: true,
        }
    }
}

/// A `CONTAINS` match on one dimension. GA4's filter grammar is a deep union
/// type; only the one shape `search_*` needs is modelled here, because a
/// half-built filter tree is harder to read than the JSON it produces.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StringFilter {
    pub match_type: String,
    pub value: String,
    pub case_sensitive: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldFilter {
    pub field_name: String,
    pub string_filter: StringFilter,
}

#[derive(Serialize)]
pub struct DimensionFilter {
    pub filter: FieldFilter,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub date_ranges: Vec<DateRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<Named>,
    pub metrics: Vec<Named>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order_bys: Vec<OrderBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension_filter: Option<DimensionFilter>,
}

impl ReportRequest {
    pub fn new(metrics: &[&str]) -> ReportRequest {
        ReportRequest {
            date_ranges: Vec::new(),
            dimensions: Vec::new(),
            metrics: Named::list(metrics),
            limit: None,
            order_bys: Vec::new(),
            dimension_filter: None,
        }
    }

    pub fn range(mut self, range: DateRange) -> Self {
        self.date_ranges = vec![range];
        self
    }

    pub fn by(mut self, dimensions: &[&str]) -> Self {
        self.dimensions = Named::list(dimensions);
        self
    }

    pub fn top(mut self, metric: &str, limit: i32) -> Self {
        self.order_bys = vec![OrderBy::desc(metric)];
        self.limit = Some(limit);
        self
    }

    /// Keep only rows whose `dimension` contains `needle`, case-insensitively —
    /// what somebody typing a fragment of a URL or an event name expects.
    pub fn containing(mut self, dimension: &str, needle: &str) -> Self {
        self.dimension_filter = Some(DimensionFilter {
            filter: FieldFilter {
                field_name: dimension.to_string(),
                string_filter: StringFilter {
                    match_type: "CONTAINS".to_string(),
                    value: needle.to_string(),
                    case_sensitive: false,
                },
            },
        });
        self
    }
}

// --------------------------------------------------------------- response ---

#[derive(Deserialize, Default)]
#[allow(dead_code)]
pub struct Header {
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct Cell {
    #[serde(default)]
    pub value: String,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    #[serde(default)]
    pub dimension_values: Vec<Cell>,
    #[serde(default)]
    pub metric_values: Vec<Cell>,
}

impl Row {
    pub fn dimension(&self, i: usize) -> &str {
        self.dimension_values
            .get(i)
            .map(|c| c.value.as_str())
            .unwrap_or("(none)")
    }

    pub fn metric(&self, i: usize) -> f64 {
        self.metric_values
            .get(i)
            .and_then(|c| c.value.parse::<f64>().ok())
            .unwrap_or(0.0)
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // headers/row_count kept for forthcoming table views
pub struct Report {
    #[serde(default)]
    pub dimension_headers: Vec<Header>,
    #[serde(default)]
    pub metric_headers: Vec<Header>,
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub totals: Vec<Row>,
    #[serde(default)]
    pub row_count: i64,
}

impl Report {
    /// Value of metric `i` summed across the whole report, as GA computed it.
    /// Falls back to summing rows when the API omits a totals row.
    pub fn total(&self, i: usize) -> f64 {
        if let Some(row) = self.totals.first() {
            return row.metric(i);
        }
        self.rows.iter().map(|r| r.metric(i)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

// ----------------------------------------------------------------- client ---

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Deserialize)]
#[allow(dead_code)] // `status` is useful when debugging raw API errors
struct ApiErrorBody {
    message: String,
    #[serde(default)]
    status: String,
}

pub struct Ga {
    http: reqwest::Client,
    auth: Auth,
}

impl Ga {
    pub fn new() -> Result<Ga> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("anacraft/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let auth = Auth::new(http.clone())?;
        Ok(Ga { http, auth })
    }

    /// The credential store behind this client, so a command that needs a
    /// wider scope than reporting can ask for one before it starts.
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        let token = self.auth.access_token().await?;
        let res = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .context("calling the Google Analytics API")?;

        let status = res.status();
        let text = res.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("{}", explain(status.as_u16(), &text));
        }

        serde_json::from_str(&text).context("unexpected response shape from Google")
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let token = self.auth.access_token().await?;
        let res = self.http.get(url).bearer_auth(token).send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("{}", explain(status.as_u16(), &text));
        }
        serde_json::from_str(&text).context("unexpected response shape from Google")
    }

    /// Run a report, transparently retrying with the legacy `conversions`
    /// metric for properties that predate the `keyEvents` rename.
    pub async fn report(&self, property: &str, req: ReportRequest) -> Result<Report> {
        let url = format!("{DATA_API}/properties/{property}:runReport");
        match self.post::<Report>(&url, &req).await {
            Ok(report) => Ok(report),
            Err(err) => {
                let msg = err.to_string();
                let uses_key_events = req.metrics.iter().any(|m| m.name == "keyEvents");
                if uses_key_events && msg.contains("keyEvents") {
                    let mut retry = req;
                    for metric in retry.metrics.iter_mut() {
                        if metric.name == "keyEvents" {
                            metric.name = "conversions".to_string();
                        }
                    }
                    for order in retry.order_bys.iter_mut() {
                        if order.metric.metric_name == "keyEvents" {
                            order.metric.metric_name = "conversions".to_string();
                        }
                    }
                    return self.post::<Report>(&url, &retry).await;
                }
                Err(err)
            }
        }
    }

    pub async fn realtime(&self, property: &str, req: ReportRequest) -> Result<Report> {
        let url = format!("{DATA_API}/properties/{property}:runRealtimeReport");
        self.post::<Report>(&url, &req).await
    }

    /// Every property the signed-in account can read.
    pub async fn properties(&self) -> Result<Vec<Property>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!("{ADMIN_API}/accountSummaries?pageSize=200");
            if let Some(token) = &page_token {
                url.push_str(&format!("&pageToken={token}"));
            }
            let page: AccountSummaries = self.get(&url).await?;

            for account in page.account_summaries {
                for prop in account.property_summaries {
                    out.push(Property {
                        id: prop.property.trim_start_matches("properties/").to_string(),
                        name: prop.display_name,
                        account: account.display_name.clone(),
                    });
                }
            }

            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(out)
    }
}

pub struct Property {
    pub id: String,
    pub name: String,
    pub account: String,
}

/// A GA4 account — the container a property is created inside.
pub struct Account {
    /// Bare numeric id, e.g. "1234". The API wants it back as `accounts/1234`.
    pub id: String,
    pub name: String,
}

impl Account {
    /// The resource name a `parent` field expects.
    pub fn parent(&self) -> String {
        format!("accounts/{}", self.id)
    }
}

/// A web data stream: the thing that owns a measurement id.
pub struct WebStream {
    pub measurement_id: String,
    pub default_uri: String,
}

/// An event the property has been told to treat as an outcome.
pub struct KeyEvent {
    pub name: String,
    /// Whether it was defined by hand rather than shipped by GA4. Kept because
    /// a custom key event that never fires is a broken implementation, while a
    /// built-in one that never fires may just not apply to this site.
    pub custom: bool,
}

impl Ga {
    /// Every GA4 account this login can act in.
    ///
    /// Distinct from `properties()`, which reads account *summaries* for their
    /// property lists. Creating needs the account id itself, and an account
    /// with no properties yet — the common case for somebody setting up their
    /// first site — has no summary worth reading.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!("{ADMIN_API}/accounts?pageSize=200");
            if let Some(token) = &page_token {
                url.push_str(&format!("&pageToken={token}"));
            }
            let page: AccountList = self.get(&url).await?;

            for account in page.accounts {
                out.push(Account {
                    id: account.name.trim_start_matches("accounts/").to_string(),
                    name: account.display_name,
                });
            }

            match page.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(out)
    }

    /// The web data streams on a property. App streams are dropped: they carry
    /// no measurement id, and nothing here can put a tag on a phone.
    ///
    /// Unpaginated on purpose — GA4 caps a property at 50 data streams, so one
    /// page of 200 is all of them.
    pub async fn web_streams(&self, property: &str) -> Result<Vec<WebStream>> {
        let url = format!("{ADMIN_API}/properties/{property}/dataStreams?pageSize=200");
        let page: DataStreamList = self.get(&url).await?;
        Ok(page
            .data_streams
            .into_iter()
            .filter_map(|stream| {
                let web = stream.web_stream_data?;
                Some(WebStream {
                    measurement_id: web.measurement_id,
                    default_uri: web.default_uri,
                })
            })
            .collect())
    }

    /// The events this property counts as key events.
    ///
    /// Configuration rather than measurement, and the two answer different
    /// questions: the Data API says how often `purchase` fired, this says
    /// whether anybody ever told GA4 that `purchase` was the point. A property
    /// with traffic and no key events is collecting fine and reporting
    /// nothing, which is the single most common thing `craft audit` finds.
    ///
    /// Unpaginated on purpose — GA4 caps a property at 30 key events, so one
    /// page of 200 is all of them.
    ///
    /// v1beta answers `keyEvents`; properties that predate the rename answer
    /// `conversionEvents` on a path of the same name and 404 the new one. Same
    /// fallback [`Ga::report`] makes for the metric, and the same reason. When
    /// both fail the first error is the one returned: the modern path is the
    /// one whose message is worth reading.
    pub async fn key_events(&self, property: &str) -> Result<Vec<KeyEvent>> {
        let url = |path: &str| format!("{ADMIN_API}/properties/{property}/{path}?pageSize=200");

        let page: KeyEventList = match self.get(&url("keyEvents")).await {
            Ok(page) => page,
            Err(modern) => self
                .get(&url("conversionEvents"))
                .await
                .map_err(|_legacy| modern)?,
        };

        Ok(page
            .key_events
            .into_iter()
            .chain(page.conversion_events)
            .filter(|event| !event.event_name.is_empty())
            .map(|event| KeyEvent {
                name: event.event_name,
                custom: event.custom,
            })
            .collect())
    }

    /// Create a property. Requires `analytics.edit`.
    pub async fn create_property(
        &self,
        account: &Account,
        display_name: &str,
        time_zone: &str,
        currency: &str,
    ) -> Result<Property> {
        let url = format!("{ADMIN_API}/properties");
        let body = serde_json::json!({
            "parent": account.parent(),
            "displayName": display_name,
            "timeZone": time_zone,
            "currencyCode": currency,
        });
        let created: PropertyResource = self.post(&url, &body).await?;
        Ok(Property {
            id: created.name.trim_start_matches("properties/").to_string(),
            name: created.display_name,
            account: account.name.clone(),
        })
    }

    /// Create the web data stream that mints the measurement id. Requires
    /// `analytics.edit`.
    pub async fn create_web_stream(
        &self,
        property: &str,
        display_name: &str,
        default_uri: &str,
    ) -> Result<WebStream> {
        let url = format!("{ADMIN_API}/properties/{property}/dataStreams");
        let body = serde_json::json!({
            "type": "WEB_DATA_STREAM",
            "displayName": display_name,
            "webStreamData": { "defaultUri": default_uri },
        });
        let created: DataStreamResource = self.post(&url, &body).await?;
        let web = created.web_stream_data.context(
            "Google created the stream but returned no web stream data, so there is no \
             measurement id to print — check the property in the Analytics console",
        )?;
        Ok(WebStream {
            measurement_id: web.measurement_id,
            default_uri: web.default_uri,
        })
    }

    /// Move a property to the Analytics trash. Requires `analytics.edit`.
    ///
    /// The one destructive call in this client, and it is here because people
    /// asked for it by name: a property `craft configure` created in one
    /// command should not need four console screens to take back. It is only
    /// ever reached from `craft delete --all`, which is opt-in twice over —
    /// the command, and then the flag.
    ///
    /// What keeps that defensible is that Google's delete is a soft one. The
    /// property goes to the account's trash and sits there for 35 days, fully
    /// restorable from the console, before anything is actually gone. The undo
    /// is Google's, it is where a person would look for it, and no code here
    /// can shorten it.
    ///
    /// `docs/oauth-scopes.md` is the submission this app is verified against
    /// and has to keep describing what the code does — see the test below,
    /// which fails the build if a second destructive verb appears.
    pub async fn delete_property(&self, property: &str) -> Result<()> {
        let url = format!("{ADMIN_API}/properties/{property}");
        let token = self.auth.access_token().await?;
        let res = self
            .http
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("calling the Google Analytics API")?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            bail!("{}", explain(status.as_u16(), &text));
        }
        Ok(())
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountList {
    #[serde(default)]
    accounts: Vec<AccountResource>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountResource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PropertyResource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DataStreamList {
    #[serde(default)]
    data_streams: Vec<DataStreamResource>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DataStreamResource {
    /// Absent on app streams, which is how they get filtered out.
    #[serde(default)]
    web_stream_data: Option<WebStreamData>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WebStreamData {
    #[serde(default)]
    measurement_id: String,
    #[serde(default)]
    default_uri: String,
}

/// One shape for both spellings, so the fallback does not need two of these.
/// Whichever list the API filled, the other stays empty and chains to nothing.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KeyEventList {
    #[serde(default)]
    key_events: Vec<KeyEventResource>,
    #[serde(default)]
    conversion_events: Vec<KeyEventResource>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KeyEventResource {
    #[serde(default)]
    event_name: String,
    #[serde(default)]
    custom: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountSummaries {
    #[serde(default)]
    account_summaries: Vec<AccountSummary>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AccountSummary {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    property_summaries: Vec<PropertySummary>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PropertySummary {
    #[serde(default)]
    property: String,
    #[serde(default)]
    display_name: String,
}

/// Turn Google's error payloads into something a user can act on.
fn explain(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<ApiError>(body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| body.chars().take(300).collect());

    match status {
        401 => format!("login expired — run `craft login`\n  ({detail})"),
        403 if detail.contains("has not been used") || detail.contains("is disabled") => format!(
            "an API isn't enabled on your Google Cloud project.\n  \
             Enable both the Google Analytics Data API and Admin API, then retry.\n  ({detail})"
        ),
        403 if detail.contains("insufficient authentication scopes") => format!(
            "this login has not granted permission to change your Analytics setup.\n  \
             Run `craft configure <domain>` again and approve the screen Google shows.\n  ({detail})"
        ),
        403 => format!(
            "access denied — the signed-in account needs at least Viewer on this property,\n  \
             or Editor on the account to create one.\n  ({detail})"
        ),
        429 => format!("Google rate-limited this request; try again shortly.\n  ({detail})"),
        _ => format!("Google Analytics API error {status}: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source of this module, read at compile time.
    ///
    /// Scanning it is unusual, and deliberate. `docs/oauth-scopes.md` tells
    /// Google's verification reviewers that the `analytics.edit` grant is only
    /// ever used to create — that nothing here modifies or deletes anything in
    /// somebody's Analytics account. That is a promise about the whole binary,
    /// not about one function, and the only way to keep it true as this file
    /// grows is to fail the build when it stops being true.
    const SOURCE: &str = include_str!("ga.rs");

    /// Everything above this test module — the part that can actually issue a
    /// request. Scanning the whole file would match this test's own list of
    /// forbidden verbs.
    fn client_source() -> &'static str {
        SOURCE
            .split_once("\n#[cfg(test)]")
            .map(|(code, _)| code)
            .expect("this module is the first test module in the file")
    }

    #[test]
    fn the_admin_api_surface_is_two_creates_and_one_named_delete() {
        let source = client_source();

        // Nothing here modifies a resource somebody else made. A property's
        // settings, its streams, its user links: anacraft reads them and never
        // writes over them.
        for verb in [".patch(", ".put("] {
            assert!(
                !source.contains(verb),
                "a `{verb}` request appeared in the Analytics client. If that is \
                 intentional, the scope justification in docs/oauth-scopes.md no \
                 longer describes what this binary does, and Google was told \
                 otherwise — update the submission before shipping it."
            );
        }

        // One destructive call, and one only: `properties.delete`, behind
        // `craft delete --all`. The same warning applies to a second one.
        assert_eq!(
            source.matches(".delete(").count(),
            1,
            "the Analytics client should issue exactly one DELETE — \
             properties.delete, from `delete_property`. Anything else is a \
             widening of what docs/oauth-scopes.md told Google this app does."
        );

        // And the write path is the documented three, not a fourth thing that
        // grew in beside them.
        let writes: Vec<&str> = source
            .lines()
            .map(|line| line.trim_start())
            .filter(|line| {
                line.starts_with("pub async fn create_") || line.starts_with("pub async fn delete_")
            })
            .collect();
        assert_eq!(
            writes.len(),
            3,
            "expected exactly properties.create, dataStreams.create and \
             properties.delete, got: {writes:?}"
        );
    }

    #[test]
    fn an_account_id_becomes_the_parent_the_api_expects() {
        let account = Account {
            id: "1234".to_string(),
            name: "Anacraft".to_string(),
        };
        assert_eq!(account.parent(), "accounts/1234");
    }

    #[test]
    fn app_streams_are_dropped_because_they_carry_no_measurement_id() {
        // dataStreams.list returns web and app streams together. An app stream
        // has no webStreamData at all, and treating one as a match would print
        // an empty tag.
        let page: DataStreamList = serde_json::from_str(
            r#"{"dataStreams":[
                 {"displayName":"iOS","androidAppStreamData":{}},
                 {"displayName":"example.com","webStreamData":
                   {"measurementId":"G-1A2BCD345E","defaultUri":"https://example.com"}}
               ]}"#,
        )
        .unwrap();

        let web: Vec<&DataStreamResource> = page
            .data_streams
            .iter()
            .filter(|s| s.web_stream_data.is_some())
            .collect();
        assert_eq!(web.len(), 1);
        let data = web[0].web_stream_data.as_ref().unwrap();
        assert_eq!(data.measurement_id, "G-1A2BCD345E");
        assert_eq!(data.default_uri, "https://example.com");
    }

    #[test]
    fn both_spellings_of_the_key_event_list_read_the_same() {
        // v1beta renamed conversionEvents to keyEvents and kept the old path
        // alive for properties that predate it. One struct reads both, so the
        // fallback in `key_events` only has to change the URL.
        let modern: KeyEventList = serde_json::from_str(
            r#"{"keyEvents":[{"eventName":"purchase","custom":false},
                             {"eventName":"demo_booked","custom":true}]}"#,
        )
        .unwrap();
        let legacy: KeyEventList = serde_json::from_str(
            r#"{"conversionEvents":[{"eventName":"purchase","custom":false},
                                    {"eventName":"demo_booked","custom":true}]}"#,
        )
        .unwrap();

        let names = |page: KeyEventList| -> Vec<String> {
            page.key_events
                .into_iter()
                .chain(page.conversion_events)
                .map(|e| e.event_name)
                .collect()
        };
        assert_eq!(names(modern), vec!["purchase", "demo_booked"]);
        assert_eq!(names(legacy), vec!["purchase", "demo_booked"]);
    }

    #[test]
    fn a_missing_scope_is_explained_as_the_command_that_fixes_it() {
        let body = r#"{"error":{"message":"Request had insufficient authentication scopes."}}"#;
        let message = explain(403, body);
        assert!(message.contains("craft configure"), "got: {message}");
    }
}
