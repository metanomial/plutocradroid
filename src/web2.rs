use std::fmt::Display;
use std::fmt;
use rocket_oauth2::{OAuth2, TokenResponse};
use rocket::http::{Cookies, Cookie, SameSite};
use rocket::response::{Responder, Redirect};
use rocket::request::{FromRequest,Request,Outcome};
use rocket::request::LenientForm;
use rocket::response::content::Content;
use rocket::response::Response;
use rocket::http::{ContentType, RawStr, Status};
use rocket::fairing;
use maud::{html, Markup};
use diesel::prelude::*;
use chrono::{DateTime, Utc, SecondsFormat, TimeZone};

use crate::{schema, rocket_diesel};
use crate::models::{Motion, MotionVote, MotionWithCount};

fn generate_state<A: rand::RngCore + rand::CryptoRng>(rng: &mut A) -> Result<String, String> {
    let mut buf = [0; 16]; // 128 bits
    rng.try_fill_bytes(&mut buf).map_err(|_| {
        String::from("Failed to generate random data")
    })?;
    Ok(base64::encode_config(&buf, base64::URL_SAFE_NO_PAD))
}

#[derive(Debug,Copy,Clone,PartialEq,Eq)]
enum MotionListFilter {
    All,
    Passed,
    Failed,
    Finished,
    Pending,
    PendingPassed,
}

impl Default for MotionListFilter {
    fn default() -> Self {
        MotionListFilter::All
    }
}

impl<'v> rocket::request::FromFormValue<'v> for MotionListFilter {
    type Error = &'v RawStr;
    fn from_form_value(v: &'v RawStr) -> Result<Self, Self::Error> {
        match v.as_str() {
            "all" => Ok(Self::All),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "finished" => Ok(Self::Finished),
            "pending" => Ok(Self::Pending),
            "pending_passed" => Ok(Self::PendingPassed),
            _ => Err(v)
        }
    }

    fn default() -> Option<Self> {
        Some(Default::default())
    }
}

struct DiscordOauth;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CSRFToken(pub String);

#[derive(Debug, Clone, FromForm)]
struct CSRFForm {
    csrf: String
}

#[derive(Debug, Clone, FromForm)]
struct VoteForm {
    csrf: String,
    count: i64,
    direction: String,
}

#[derive(Debug, Clone)]
struct MiscError(String);

impl Display for MiscError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MiscError {:?}", self.0)
    }
}

impl std::error::Error for MiscError {}

impl From<String> for MiscError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for MiscError {
    fn from(s: &str) -> Self {
        Self(String::from(s))
    }
}

#[derive(Deserialize,Serialize,Debug,Clone)]
struct DiscordUser {
    pub id: String,
    pub username: String,
    pub discriminator: String,
    pub avatar: String,
}

impl DiscordUser {
    pub fn id(&self) -> i64 {
        self.id.parse().unwrap()
    }
}

#[derive(Deserialize,Serialize,Debug,Clone)]
struct Deets {
    pub discord_user: DiscordUser,
}

impl Deets {
    pub fn id(&self) -> i64 {
        self.discord_user.id()
    }
}

#[derive(Debug)]
enum DeetsFail {
    BadDeets(serde_json::error::Error),
    NoDeets
}

impl <'a, 'r> FromRequest<'a, 'r> for Deets {
    type Error = DeetsFail;

    fn from_request(request: &'a Request<'r>) -> Outcome<Self, Self::Error> {
        let mut c = request.cookies();
        //let maybe_deets = c.get("deets");
        match c.get_private("deets").map(|c| serde_json::from_str(c.value()):Result<Self,_>) {
            Some(Ok(deets)) => Outcome::Success(deets),
            Some(Err(e)) => Outcome::Failure((rocket::http::Status::BadRequest,DeetsFail::BadDeets(e))),
            None => Outcome::Failure((rocket::http::Status::Unauthorized,DeetsFail::NoDeets)),
        }
    }
}

#[derive(Debug)]
enum CommonContextError {
    DeetsError(DeetsFail),
    DbConnError(()),
}

impl From<DeetsFail> for CommonContextError {
    fn from(d: DeetsFail) -> Self {
        CommonContextError::DeetsError(d)
    }
}

impl From<()> for CommonContextError {
    fn from(_: ()) -> Self {
        CommonContextError::DbConnError(())
    }
}

struct CommonContext<'a> {
    pub csrf_token: String,
    pub cookies: Cookies<'a>,
    pub deets: Option<Deets>,
    pub conn: rocket_diesel::DbConn,
}

impl<'a> core::ops::Deref for CommonContext<'a> {
    type Target = diesel::pg::PgConnection;

    fn deref(&self) -> &Self::Target {
        self.conn.deref()
    }
}

impl <'a, 'r> FromRequest<'a, 'r> for CommonContext<'a> {
    type Error = CommonContextError;

    fn from_request(request: &'a Request<'r>) -> Outcome<Self, Self::Error> {
        let mut cookies = request.cookies();
        let csrf_token = match cookies.get("csrf_protection_token") {
            Some(token) => token.value().to_string(),
            None => {
                let new_token = generate_state(&mut rand::thread_rng()).unwrap();
                cookies.add(
                    Cookie::build("csrf_protection_token", new_token.clone())
                        .same_site(SameSite::Strict)
                        .secure(true)
                        .http_only(true)
                        .finish()
                );
                new_token
            }
        };
        let deets = match cookies.get_private("deets").map(|c| serde_json::from_str(c.value()):Result<Deets,_>) {
            Some(Ok(deets)) => Some(deets),
            Some(Err(e)) => {
                warn!("Failed to parse deets, {:?}", e);
                None
            },
            None => None,
        };

        let conn = rocket_diesel::DbConn::from_request(request).map_failure(|(a,_)| (a, CommonContextError::from(())))?;
        Outcome::Success(Self{
            csrf_token,
            cookies,
            deets,
            conn,
        })
    }
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
struct SecureHeaders;

impl fairing::Fairing for SecureHeaders {
    fn info(&self) -> fairing::Info {
        fairing::Info {
            name: "Secure Headers Fairing",
            kind: fairing::Kind::Response,
        }
    }

    fn on_response(&self, _request: &Request, response: &mut Response) {
        use rocket::http::Header;
        response.adjoin_header(Header::new(
            "Content-Security-Policy",
            "default-src 'none'; frame-ancestors 'none'; img-src 'self'; script-src 'self'; style-src 'self'"
        ));
        response.adjoin_header(Header::new(
            "Referrer-Policy",
            "strict-origin-when-cross-origin"
        ));
        response.adjoin_header(Header::new(
            "X-Content-Type-Options",
            "nosniff"
        ));
        response.adjoin_header(Header::new(
            "X-Frame-Options",
            "DENY"
        ));
        // Strict-Transport-Security is purposefully omitted here; Rocket does not support SSL/TLS. The layer that is adding SSL/TLS (most likely nginx or apache) should add an appropriate STS header.
    }
}

fn motion_snippet(
    motion: &MotionWithCount
) -> Markup {
    html!{
        div.motion-titlebar {
            a href=(format!("/motions/{}", motion.damm_id())) {
                h3.motion-title { "Motion #" (motion.damm_id())}
            }
            span.motion-time {
                @if motion.announcement_message_id.is_some() {
                    @if motion.is_win {
                        "PASSED"
                    } @else {
                        "FAILED"
                    }
                    " at "
                } @else {
                    " will "
                    @if motion.is_win {
                        "pass"
                    } @else {
                        "fail"
                    }
                    " at"
                    abbr title="assuming no other result changes" { "*" }
                    " "
                }
                time datetime=(motion.end_at().to_rfc3339()) {
                    (motion.end_at().to_rfc2822())
                }
            }
        }
        p.motion-text {
            @if motion.is_super {
                "Super motion "
            } @else {
                "Simple motion "
            }
            (motion.motion_text)
        }
        div {
            @if motion.is_win {
                span.winner {
                    (motion.yes_vote_count)
                    " for "
                }
                "vs"
                span.loser {
                    " against "
                    (motion.no_vote_count)
                }
            } @else {
                span.winner {
                    (motion.no_vote_count)
                    " against "
                }
                "vs"
                span.loser {
                    " for "
                    (motion.yes_vote_count)
                }
            }
        }
    }
}

fn page(ctx: &mut CommonContext, title: impl AsRef<str>, content: Markup) -> Markup {
    use schema::item_types::dsl as itdsl;
    use crate::view_schema::balance_history::dsl as bhdsl;
    bare_page(title, html!{
        @if let Some(deets) = ctx.deets.as_ref() {
            @let item_types:Vec<String> = itdsl::item_types.select(itdsl::name).get_results(&**ctx).unwrap();
            @let id:i64 = deets.discord_user.id();
            @let balances = item_types.iter().map(|name| {
                (name,bhdsl::balance_history
                    .select(bhdsl::balance)
                    .filter(bhdsl::user.eq(id))
                    .filter(bhdsl::ty.eq(name))
                    .order(bhdsl::happened_at.desc())
                    .limit(1)
                    .get_result(&**ctx)
                    .optional()
                    .unwrap() //unwrap Result (query might fail)
                    .unwrap_or(0) //unwrap Option (row might not exist)
                )
            });
            p { "Welcome, " (deets.discord_user.username) "#" (deets.discord_user.discriminator)}
            form action="/logout" method="post" {
                input type="hidden" name="csrf" value=(ctx.csrf_token);
                input type="submit" name="submit" value="Logout";
            }
            ul {
                @for (name, amount) in balances {
                    li { (amount) (name) }
                }
            }
            a href="/" { "Home" }
            " | "
            a href="/my-transactions" { "My Transactions" }
        } @else {
            form action="/login/discord" method="post" {
                input type="hidden" name="csrf" value=(ctx.csrf_token);
                p {
                    "I don't know who you are. You should "
                    input type="submit" name="submit" value="Login";
                }
            }
        }
        (content)
    })
}

fn bare_page(title: impl AsRef<str>, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html {
            head {
                title { (title.as_ref()) }
                meta charset="utf-8";
                meta name="viewport" content="width = device-width, initial-scale = 1";
                link rel="stylesheet" href={"/" (static_path!(main.css))};
                link rel="icon" type="image/png" href={"/" (static_path!(favicon.png))};
            }
            body {
                div.container {
                    (content)
                    small.build-info {
                        "Plutocradroid "
                        (env!("VERGEN_SEMVER_LIGHTWEIGHT"))
                        " commit "
                        (env!("VERGEN_SHA_SHORT"))
                        " built for "
                        (env!("VERGEN_TARGET_TRIPLE"))
                        " at "
                        (env!("VERGEN_BUILD_TIMESTAMP"))
                    }
                }
            }
        }
    }
}

#[post("/motions/<damm_id>/vote", data = "<data>")]
fn motion_vote(
    mut ctx: CommonContext,
    data: LenientForm<VoteForm>,
    damm_id: String,
) -> impl Responder<'static> {
    let id:i64;
    if let Some(digits) = crate::damm::validate_ascii(damm_id.as_str()) {
        id = atoi::atoi(digits.as_slice()).unwrap();
    } else {
        info!("bad id");
        return Err(rocket::http::Status::NotFound);
    }
    if ctx.cookies.get("csrf_protection_token").map(|token| token.value()) != Some(data.csrf.as_str()) {
        return Err(rocket::http::Status::BadRequest);
    }
    let deets:&Deets;
    if let Some(d) = ctx.deets.as_ref() {
        deets = d;
    } else {
        info!("no deets");
        return Err(rocket::http::Status::Unauthorized);
    }
    let vote_count = data.count;
    let vote_direction:bool;
    if data.direction.as_str() == "for" {
        vote_direction = true;
    } else if data.direction.as_str() == "against" {
        vote_direction = false;
    } else {
        info!("bad vote direction {:?}", data.direction);
        return Err(rocket::http::Status::BadRequest);
    }
    let resp = crate::bot::vote_common(
        &ctx.conn,
        Some(vote_direction),
        vote_count,
        deets.discord_user.id(),
        Some(id),
        None,
        None
    );

    Ok(page(&mut ctx, "Vote Complete", html!{
        (resp)
        br;
        a href={"/motions/" (damm_id)} { "Back to Motion" }
        br;
        a href="/" { "Back Home" }
    }))
}

#[get("/motions/<damm_id>")]
fn motion_listing(mut ctx: CommonContext, damm_id: String) -> impl Responder<'static> {
    let id:i64;
    if let Some(digits) = crate::damm::validate_ascii(damm_id.as_str()) {
        id = atoi::atoi(digits.as_slice()).unwrap();
    } else {
        return None;
    }

    use schema::motions::dsl as mdsl;
    use schema::motion_votes::dsl as mvdsl;
    let maybe_motion:Option<Motion> = mdsl::motions.select((
        mdsl::rowid,
        mdsl::bot_message_id,
        mdsl::motion_text,
        mdsl::motioned_at,
        mdsl::last_result_change,
        mdsl::is_super,
        mdsl::announcement_message_id,
    )).filter(mdsl::rowid.eq(id)).get_result(&*ctx).optional().unwrap();

    let motion;
    if let Some(m) = maybe_motion {
        motion = m;
    }else{
        return None;
    }

    let votes:Vec<MotionVote> = mvdsl::motion_votes
        .select((mvdsl::user, mvdsl::direction, mvdsl::amount))
        .filter(mvdsl::motion.eq(motion.rowid))
        .get_results(&*ctx)
        .unwrap();
    let (yes_vote_count, no_vote_count) = votes
        .iter()
        .map(|v| if v.direction { (v.amount, 0) } else { (0, v.amount) })
        .fold((0,0), |acc, x| (acc.0 + x.0, acc.1 + x.1));
    let motion = MotionWithCount::from_motion(motion, yes_vote_count as u64, no_vote_count as u64);
    let voting_html = if let Some(deets) = ctx.deets.as_ref(){
        if motion.end_at() > Utc::now() {
            let mut agents_vote:Option<MotionVote> = None;
            for vote in &votes {
                if vote.user == atoi::atoi::<i64>(deets.discord_user.id.as_bytes()).unwrap() {
                    agents_vote = Some(*vote);
                }
            }
            let avd = agents_vote.map(|v| v.direction);
            html!{
                form action={"/motions/" (damm_id) "/vote"} method="post" {
                    input type="hidden" name="csrf" value=(ctx.csrf_token);
                    "Cast "
                    input type="number" name="count" value="0";
                    " vote(s) "
                    br;
                    label {
                    input type="radio" name="direction" value="for" disabled?[avd == Some(false)] checked?[avd == Some(true)];
                    " for"
                    }
                    br;
                    label {
                        input type="radio" name="direction" value="against" disabled?[avd == Some(true)] checked?[avd == Some(false)];
                        " against"
                    }
                    br;
                    input type="submit" name="submit" value="Go";
                }
            }
        } else {
            html!{ "This motion has expired." }
        }
    } else {
        html!{ "You must be logged in to vote." }
    };

    Some(page(&mut ctx, format!("Motion #{}", motion.damm_id()), html!{
        div.motion {
            a href="/" { "Home" }
            (motion_snippet(&motion))
            hr;
            (voting_html)
            hr;
            @for vote in &votes {
                div.motion-vote {
                    h5 { (vote.user) }
                    span {
                        (vote.amount)
                        @if vote.direction {
                            " for"
                        } @else {
                            " against"
                        }
                    }
                }
            }
        }
    }))
}

#[get("/?<filter>")]
fn index(mut ctx: CommonContext, filter: MotionListFilter) -> impl Responder<'static> {
    use schema::motions::dsl as mdsl;
    use schema::motion_votes::dsl as mvdsl;
    let bare_motions:Vec<Motion> = mdsl::motions
        .select((
            mdsl::rowid,
            mdsl::bot_message_id,
            mdsl::motion_text,
            mdsl::motioned_at,
            mdsl::last_result_change,
            mdsl::is_super,
            mdsl::announcement_message_id,
        ))
        .order((mdsl::announcement_message_id.is_null().desc(), mdsl::rowid.desc()))
        .get_results(&*ctx)
        .unwrap();

    let get_vote_count = |motion_id:i64, dir:bool| -> Result<i64, diesel::result::Error> {
        use bigdecimal::{BigDecimal,ToPrimitive};
        let votes:Option<BigDecimal> = mvdsl::motion_votes
        .select(diesel::dsl::sum(mvdsl::amount))
        .filter(mvdsl::motion.eq(motion_id))
        .filter(mvdsl::direction.eq(dir))
        .get_result(&*ctx)?;
        Ok(votes.map(|bd| bd.to_i64().unwrap()).unwrap_or(0))
    };

    let all_motions = (bare_motions.into_iter().map(|m| {
        let yes_votes = get_vote_count(m.rowid, true)?;
        let no_votes = get_vote_count(m.rowid, false)?;
        Ok(MotionWithCount::from_motion(m, yes_votes as u64, no_votes as u64))
    }).collect():Result<Vec<_>,diesel::result::Error>).unwrap().into_iter();

    let motions = match filter {
        MotionListFilter::All => all_motions.collect(),

        MotionListFilter::Failed =>
            all_motions.filter(|m| m.announcement_message_id.is_some() && !m.is_win).collect(),

        MotionListFilter::Finished =>
            all_motions.filter(|m| m.announcement_message_id.is_some()).collect(),

        MotionListFilter::Passed =>
            all_motions.filter(|m| m.announcement_message_id.is_some() &&  m.is_win).collect(),

        MotionListFilter::Pending =>
            all_motions.filter(|m| m.announcement_message_id.is_none()).collect(),

        MotionListFilter::PendingPassed =>
            all_motions.filter(|m| m.announcement_message_id.is_none() ||  m.is_win).collect(),
    }:Vec<_>;

    page(&mut ctx, "All Motions", html!{
        form#filters method="get" {
            div {
                "Filters:"
                ul {
                    @let options = [
                        ("all", "All", MotionListFilter::All),
                        ("passed", "Passed", MotionListFilter::Passed),
                        ("failed", "Failed", MotionListFilter::Failed),
                        ("finished", "Finished (Passed or Failed)", MotionListFilter::Finished),
                        ("pending", "Pending", MotionListFilter::Pending),
                        ("pending_passed", "Pending or Passed", MotionListFilter::PendingPassed),
                    ];
                    @for (codename, textname, val) in &options {
                        li {
                            label {
                                input type="radio" name="filter" value=(codename) checked?[filter == *val];
                                (textname)
                            }
                        }
                    }
                }
                input type="submit" name="submit" value="Go";
            }
        }
        @for motion in &motions {
            div.motion {
                (motion_snippet(&motion))
            }
        }
        @if motions.is_empty() {
            p.no-motions { "Nobody here but us chickens!" }
        }
    })
}

sql_function!{
    #[sql_name = "coalesce"]
    fn coalesce_2<T: diesel::sql_types::NotNull>(a: diesel::sql_types::Nullable<T>, b: T) -> T;
}
// use diesel::sql_types::Bool;
// sql_function!{
//     #[sql_name = "coalesce"]
//     fn coalesce_2_bool(a: diesel::sql_types::Nullable<Bool>, b: Bool) -> Bool;
// }

#[get("/my-transactions?<before_ms>&<fun_ty>")]
fn my_transactions(
    mut ctx: CommonContext,
    fun_ty: Option<String>,
    before_ms: Option<i64>,
) -> Result<Markup, Status> {
    use crate::view_schema::balance_history::dsl as bh;
    use crate::schema::item_types::dsl as it;
    let before_ms = before_ms.unwrap_or(i64::MAX);
    #[cfg(feature = "debug")]
    let limit = 10;
    #[cfg(not(feature = "debug"))]
    let limit = 1000;
    let fun_ty_string = fun_ty.unwrap_or_else(|| String::from("all"));
    #[derive(Debug,Clone,PartialEq,Eq)]
    enum FungibleSelection {
        All,
        Specific(String),
    }
    
    impl FungibleSelection {
        pub fn as_str(&self) -> &str {
            match self {
                FungibleSelection::All => "all",
                FungibleSelection::Specific(s) => s,
            }
        }

        pub fn as_option(&self) -> Option<&str> {
            match self {
                FungibleSelection::All => None,
                FungibleSelection::Specific(s) => Some(s.as_str()),
            }
        }
    }
    #[derive(Debug,Clone,Queryable)]
    struct Transaction {
        pub rowid:i64,
        pub balance:i64,
        pub quantity:i64,
        pub sign:i32,
        pub happened_at:DateTime<Utc>,
        pub ty:String,
        pub comment:Option<String>,
        pub other_party:Option<i64>,
        pub to_motion:Option<i64>,
        pub to_votes:Option<i64>,
        pub message_id:Option<i64>,
        pub transfer_ty:String,
    }
    #[derive(Debug,Clone)]
    enum TransactionView {
        Generated{amt: i64, bal: i64},
        Trans(Transaction),
    }
    let fun_tys:Vec<String> = it::item_types.select(it::name).get_results(&*ctx).unwrap();
    let fun_ty = if fun_ty_string == "all" {
        FungibleSelection::All
    } else if fun_tys.iter().any(|ft| ft.as_str() == fun_ty_string) {
        FungibleSelection::Specific(fun_ty_string)
    } else {
        return Err(Status::BadRequest)
    };
    let txns:Option<(Vec<_>,bool)> = ctx.deets.as_ref().map(|deets| {
        let q = bh::balance_history
            .select((
                bh::rowid,
                bh::balance,
                bh::quantity,
                bh::sign,
                bh::happened_at,
                bh::ty,
                bh::comment,
                bh::other_party,
                bh::to_motion,
                bh::to_votes,
                bh::message_id,
                bh::transfer_ty,
            ))
            .filter(bh::user.eq(deets.id()))
            .filter(coalesce_2(bh::ty.nullable().eq(fun_ty.as_option()).nullable(), true))
            .filter(coalesce_2(bh::happened_at.nullable().lt(Utc.timestamp_millis_opt(before_ms).single()).nullable(),true))
            .filter(bh::transfer_ty.ne("generated"))
            .order(bh::happened_at.desc())
            .limit(limit+1);
        info!("{}", diesel::debug_query(&q));
        let txns:Vec<Transaction> = q.get_results(&*ctx)
            .unwrap();
        info!("{} txns results", txns.len());
        let mut gen_txns:Vec<Transaction> = if let [.., last] = txns.as_slice() {
            bh::balance_history
                .select((
                    bh::rowid,
                    bh::balance,
                    bh::quantity,
                    bh::sign,
                    bh::happened_at,
                    bh::ty,
                    bh::comment,
                    bh::other_party,
                    bh::to_motion,
                    bh::to_votes,
                    bh::message_id,
                    bh::transfer_ty,
                ))
                .filter(bh::user.eq(deets.id()))
                .filter(coalesce_2(bh::ty.nullable().eq(fun_ty.as_option()).nullable(), true))
                .filter(coalesce_2(bh::happened_at.nullable().lt(Utc.timestamp_millis_opt(before_ms).single()).nullable(),true))
                .filter(bh::happened_at.gt(last.happened_at))
                .filter(bh::transfer_ty.eq("generated"))
                .order(bh::happened_at.desc())
                .get_results(&*ctx)
                .unwrap()
        } else { Vec::new() };
        let mut txn_views = Vec::new();
        let (hit_limit,iter) = if txns.len() == ((limit+1) as usize) {
            (true, txns[..txns.len()-1].iter())
        } else { (false, txns.iter()) };
        for txn in iter.rev() {
            let mut amt = 0;
            let mut bal = 0;
            while gen_txns.last().map(|t| t.happened_at < txn.happened_at).unwrap_or(false) {
                let gen_txn = gen_txns.pop().unwrap();
                amt += gen_txn.quantity;
                bal = gen_txn.balance;
            }
            if amt > 0 {
                txn_views.push(TransactionView::Generated{amt, bal});
            }
            txn_views.push(TransactionView::Trans(txn.clone()));
        }
        let mut amt = 0;
        let mut bal = 0;
        while let Some(gt) = gen_txns.pop() {
            amt += gt.quantity;
            bal = gt.balance;
        }
        if amt > 0 {
            txn_views.push(TransactionView::Generated{amt,bal});
        }
        txn_views.reverse();
        (txn_views, hit_limit)
    });
    Ok(page(&mut ctx, "My Transactions", html!{
        @if let Some((txns, hit_limit)) = txns {
            h3 { "My Transactions" }
            form {
                "Show transactions in"
                ul {
                    @for ft in &fun_tys {
                        li {
                            label {
                                input type="radio" name="fun_ty" value=(ft) checked?[fun_ty == FungibleSelection::Specific(ft.clone())];
                                (ft)
                            }
                        }
                    }
                    li {
                        label {
                            input type="radio" name="fun_ty" value="all" checked?[fun_ty == FungibleSelection::All];
                            "All currencies"
                        }
                    }
                }
                button { "Go" }
            }
            table border="1" {
                thead {
                    tr {
                        th { "Timestamp" }
                        th { "Description" }
                        th { "Amount" }
                        th { "Running Total" }
                    }
                }
                tbody {
                    @for txn_view in &txns {
                        @if let TransactionView::Trans(txn) = txn_view {
                            tr.transaction {
                                td {
                                    time datetime=(txn.happened_at.to_rfc3339()) {
                                        (txn.happened_at.to_rfc3339_opts(SecondsFormat::Secs, true))
                                    }
                                }
                                td {
                                    @if ["give", "admin_give"].contains(&txn.transfer_ty.as_str()) {
                                        @if txn.transfer_ty.as_str() == "admin_give" {
                                            "admin "
                                        }
                                        @if txn.sign < 0 {
                                            "transfer to "
                                        } @else {
                                            "transfer from "
                                        }
                                        "user#\u{200B}"
                                        (txn.other_party.unwrap())
                                    } @else if txn.transfer_ty.as_str() == "motion_create" {
                                        @let damm_id = crate::damm::add_to_str(txn.to_motion.unwrap().to_string());
                                        "1 vote, created "
                                        a href=(uri!(motion_listing:damm_id = &damm_id)) {
                                            "motion #"
                                            (&damm_id)
                                        }
                                    } @else if let (Some(motion_id), Some(votes)) = (&txn.to_motion, &txn.to_votes) {
                                        // transfer_ty == "motion_vote"
                                        @let damm_id = crate::damm::add_to_str(motion_id.to_string());
                                        (votes)
                                        " vote(s) on "
                                        a href=(uri!(motion_listing:damm_id = &damm_id)) {
                                            "motion #"
                                            (&damm_id)
                                        }
                                    } @else if ["admin_fabricate","command_fabricate"].contains(&txn.transfer_ty.as_str()) {
                                        "fabrication"
                                    }
                                    " "
                                    @if let Some(comment) = &txn.comment {
                                        "“" (comment) "”"
                                    }
                                }
                                td.amount.negative[txn.sign < 0] {
                                    span.paren { "(" }
                                    span.amount-inner { (txn.quantity) }
                                    span.ty { (txn.ty) }
                                    span.paren { ")" }
                                }
                                td.running-total {
                                    span.amount-inner { (txn.balance) }
                                    span.ty { (txn.ty) }
                                }
                            }
                        } @else {
                            @let (amt, bal) = match txn_view { TransactionView::Generated{amt, bal} => (amt, bal), _ => unreachable!() };
                            tr.transaction.generated {
                                td {}
                                td { "generator outputs" }
                                td.amount {
                                    span.paren { "(" }
                                    span.amount-inner { (amt) }
                                    span.ty { "pc" }
                                    span.paren { ")" }
                                }
                                td.running-total {
                                    span.amount-inner { (bal) }
                                    span.ty { "pc" }
                                }
                            }
                        }
                    }
                    @if txns.is_empty() {
                        tr {
                            td colspan="4" {
                                "Nothing to show."
                            }
                        }
                    }
                }
            }
            @if hit_limit {
                @let txn = match txns.iter().rev().find(|t| match t{TransactionView::Trans(_) => true, _=>false}) { Some(TransactionView::Trans(t)) => t, d => {dbg!(d);unreachable!()} };
                a href=(uri!(my_transactions: before_ms = txn.happened_at.timestamp_millis(), fun_ty = fun_ty.as_str())) { "Next" }
            }
        } @else {
            p { "You must be logged in to view your transactions." }
        }
    }))
}

#[get("/oauth-finish")]
fn oauth_finish(token: TokenResponse<DiscordOauth>, mut cookies: Cookies<'_>) -> Redirect {
    cookies.add_private(
        Cookie::build("token", token.access_token().to_string())
            .same_site(SameSite::Lax)
            .secure(true)
            .http_only(true)
            .finish()
    );
    if let Some(refresh) = token.refresh_token().map(|s| s.to_owned()) {
        cookies.add_private(
            Cookie::build("refresh", refresh)
                .same_site(SameSite::Lax)
                .secure(true)
                .http_only(true)
                .finish()
        )
    }
    Redirect::to("/get-deets")
}

#[get("/get-deets")]
fn get_deets(
    mut cookies: Cookies<'_>
) -> Result<Redirect, Box<dyn std::error::Error>> {
    let token;
    if let Some(val) = cookies.get_private("token") {
        token = val.value().to_string()
    } else {
        return Ok(Redirect::to("/"));
    }
    let client = reqwest::blocking::Client::new();
    let res = client.get("https://discord.com/api/v8/users/@me")
        .bearer_auth(token)
        .send()?;
    if res.status() != 200 {
        return Err(Box::new(MiscError::from("Bad status")));
    }
    let user:DiscordUser = res.json()?;
    let deets = Deets{discord_user: user};
    info!("User logged in: {:?}", deets);
    cookies.add_private(
        Cookie::build("deets", serde_json::to_string(&deets).unwrap())
            .same_site(SameSite::Lax)
            .secure(true)
            .http_only(true)
            .finish()
    );
    Ok(Redirect::to("/"))
}

#[post("/login/discord", data = "<data>")]
fn login(
    oauth2: OAuth2<DiscordOauth>,
    mut cookies: Cookies<'_>,
    data: LenientForm<CSRFForm>,
) -> Result<Redirect, rocket::http::Status> {
    if cookies.get("csrf_protection_token").map(|token| token.value()) != Some(data.csrf.as_str()) {
        return Err(rocket::http::Status::BadRequest);
    }
    Ok(oauth2.get_redirect(&mut cookies, &["identify"]).unwrap())
}

#[post("/logout", data = "<data>")]
fn logout(
    mut ctx: CommonContext,
    data: LenientForm<CSRFForm>,
) -> Result<Markup, rocket::http::Status> {
    if ctx.cookies.get("csrf_protection_token").map(|token| token.value()) != Some(data.csrf.as_str()) {
        return Err(rocket::http::Status::BadRequest);
    }
    let cookies_clone = ctx.cookies.iter().map(Clone::clone).collect():Vec<_>;
    for cookie in cookies_clone {
        ctx.cookies.remove(cookie);
    }
    Ok(bare_page("Logged out.", html!{
        p { "You have been logged out." }
        a href="/" { "Home." }
    }))
}

#[get("/motions")]
fn motions_api_compat(
    ctx: CommonContext
) -> impl Responder {
    use schema::motions::dsl as mdsl;
    use schema::motion_votes::dsl as mvdsl;
    let bare_motions:Vec<Motion> = mdsl::motions.select((
        mdsl::rowid,
        mdsl::bot_message_id,
        mdsl::motion_text,
        mdsl::motioned_at,
        mdsl::last_result_change,
        mdsl::is_super,
        mdsl::announcement_message_id,
    )).get_results(&*ctx).unwrap();

    let get_vote_count = |motion_id:i64, dir:bool| -> Result<i64, diesel::result::Error> {
        use bigdecimal::{BigDecimal,ToPrimitive};
        let votes:Option<BigDecimal> = mvdsl::motion_votes
        .select(diesel::dsl::sum(mvdsl::amount))
        .filter(mvdsl::motion.eq(motion_id))
        .filter(mvdsl::direction.eq(dir))
        .get_result(&*ctx)?;
        Ok(votes.map(|bd| bd.to_i64().unwrap()).unwrap_or(0))
    };

    let res = (bare_motions.into_iter().map(|m| {
        let yes_votes = get_vote_count(m.rowid, true)?;
        let no_votes = get_vote_count(m.rowid, false)?;
        Ok(MotionWithCount::from_motion(m, yes_votes as u64, no_votes as u64))
    }).collect():Result<Vec<_>,diesel::result::Error>).unwrap();

    Content(ContentType::JSON, serde_json::to_string(&res).unwrap())
}

pub fn main() {
    rocket::ignite()
        .manage(rocket_diesel::init_pool())
        .attach(OAuth2::<DiscordOauth>::fairing("discord"))
        .attach(SecureHeaders)
        .mount("/", super::statics::statics_routes())
        .mount("/",routes![
            index,
            oauth_finish,
            login,
            get_deets,
            motion_listing,
            motion_vote,
            motions_api_compat,
            logout,
            my_transactions,
        ])
        .launch();
}