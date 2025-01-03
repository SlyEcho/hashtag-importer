use std::collections::HashSet;
use std::hash::Hash;
use std::io;
use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

use core::num::NonZeroU32;

use anyhow::{anyhow, bail, Context, Result};
use governor::{Quota, RateLimiter};

use crate::config::*;
use crate::types::*;

const USER_AGENT: &str = concat!("hashtag-importer v", env!("CARGO_PKG_VERSION"));
const CLIENT_NAME: &str = "hashtag-importer";
const CLIENT_WEBSITE: &str = "https://github.com/anisse/hashtag-importer";

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .cookie_store(true)
        .build()
        .context("cannot build custom client")
}

pub(crate) fn create_app() -> Result<()> {
    print!("Enter your mastodon server api domain name: ");
    io::stdout().flush().context("flush")?;
    let mut server_domain = String::new();
    io::stdin()
        .read_line(&mut server_domain)
        .context("unable to read stdin")?;
    let url = reqwest::Url::parse(format!("https://{server_domain}/").as_str())
        .with_context(|| format!("{server_domain} is not a domain"))?;
    // Register the app
    let resp: ApplicationResponse = client()?
        .post(url.join("api/v1/apps")?)
        .json(&ApplicationRegistration {
            client_name: CLIENT_NAME,
            redirect_uris: OOB_URI,
            website: CLIENT_WEBSITE,
            scopes: Scope::Read,
        })
        .send()
        .context("create app post failed")?
        .json()
        .context("create app response body not valid json")?;
    dbg!(&resp);
    println!("Copy paste this into your config.toml:");
    println!("[auth]");
    println!("client_id = '{}'", resp.client_id.unwrap());
    println!("client_secret = '{}'", resp.client_secret.unwrap());
    Ok(())
}

pub(crate) fn user_auth() -> Result<()> {
    let config = load_config("config.toml")?;
    println!("Open this link in your web browser to give the app read permission from your user account:
https://{}/oauth/authorize?response_type=code&client_id={}&redirect_uri=urn:ietf:wg:oauth:2.0:oob&scope=read",
        config.server, config.auth.client_id,
    );
    println!("Paste the code your server gave you:");
    let mut code = String::new();
    io::stdin()
        .read_line(&mut code)
        .context("unable to read stdin")?;
    let token = token(
        &config.server,
        &config.auth.client_id,
        &config.auth.client_secret,
        &code.trim().to_string(),
    )?;
    println!("Update your config.toml auth section:");
    println!("[auth]");
    println!("token = '{token}'");
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    let config = load_config("config.toml")?;
    println!(
        "{} hashtags in config: {:?}",
        config.hashtag.len(),
        config.hashtag.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
    // Rate limiters
    // Only one query (hashtag fetch or import) per minute on all servers
    let lim_queries = RateLimiter::keyed(Quota::per_minute(NonZeroU32::new(1).unwrap()));
    // At most 5 post imports per remote instance per hour
    let lim_upstreams = RateLimiter::keyed(Quota::per_hour(NonZeroU32::new(5).unwrap()));
    // At most 20 post imports into our server per hour
    let lim_import = RateLimiter::direct(Quota::per_hour(NonZeroU32::new(20).unwrap()));
    // At most 4 runs per hour (average of 15min between runs)
    let lim_loop = RateLimiter::direct(Quota::per_hour(NonZeroU32::new(4).unwrap()));
    let mut imported_statuses: Vec<HashSet<String>> = vec![HashSet::new(); config.hashtag.len()];
    loop {
        for (i, hashtag) in config.hashtag.iter().enumerate() {
            if let Err(e) = import_hashtag(
                &config,
                hashtag,
                &mut imported_statuses[i],
                &lim_queries,
                &lim_upstreams,
                &lim_import,
            ) {
                println!("Hashtag {}: {e:#}", hashtag.name);
                continue;
            }
        }
        print!(".");
        let _ = io::stdout().flush(); // we really don't care if it fails
        sleep(Duration::from_secs(5 * 60));
        wait_until(&lim_loop);
        // This one can grow unbounded, shrink it to cleanup status
        lim_upstreams.shrink_to_fit();
    }
}

fn import_hashtag(
    config: &Config,
    hashtag: &Hashtag,
    imported_statuses: &mut HashSet<String>,
    lim_queries: &governor::DefaultKeyedRateLimiter<String>,
    lim_upstreams: &governor::DefaultKeyedRateLimiter<String>,
    lim_import: &governor::DefaultDirectRateLimiter,
) -> Result<()> {
    let mut remote_statuses: HashSet<String> = HashSet::new();
    for server in hashtag.sources.iter() {
        wait_until_key(lim_queries, server);
        let list = hashtags(server, "", &hashtag.name, &hashtag.any, 25)
            .with_context(|| format!("fetch remote {server} error"))?;
        remote_statuses.extend(list.into_iter().map(|s| s.url));
    }
    /* Because of the way Mastodon IDs work, we cannot kindly ask the server to give us posts
     * 'since_id': the snowflake ID variant used by mastodon contains the timestamp of the
     * post. So importing remote posts older than the latest local post means we won't see them
     * on the next iteration if we use since_id.
     */
    wait_until_key(lim_queries, &config.server);
    let list = hashtags(
        &config.server,
        &config.auth.token,
        &hashtag.name,
        &hashtag.any,
        40,
    )
    .with_context(|| format!("fetch local {} error", config.server))?;
    let local_statuses: HashSet<String> = HashSet::from_iter(list.into_iter().map(|s| s.url));
    for status in remote_statuses.difference(&local_statuses) {
        if imported_statuses.contains(status) {
            continue;
        }
        if let Err(e) = import_status(status, config, lim_queries, lim_upstreams, lim_import) {
            println!("Hashtag {}: skipping {status} : {e:#}", hashtag.name);
            continue;
        }
        println!("Hashtag {}: imported {status}", hashtag.name);
        imported_statuses.insert(status.to_string());
    }
    // Keep only the intersection between imported, and seen this iteration.
    // This is to prevent imported_status to grow unbounded
    imported_statuses.retain(|s| remote_statuses.contains(s));
    Ok(())
}

fn import_status(
    status: &str,
    config: &Config,
    lim_queries: &governor::DefaultKeyedRateLimiter<String>,
    lim_upstreams: &governor::DefaultKeyedRateLimiter<String>,
    lim_import: &governor::DefaultDirectRateLimiter,
) -> Result<()> {
    let host = reqwest::Url::parse(status)
        .context("unparseable status url")
        .and_then(|u| {
            u.host_str()
                .map(|h| h.to_string())
                .ok_or(anyhow!("no host"))
        })
        .context("bad url")?;
    if lim_upstreams.check_key(&host).is_err() {
        bail!("for now reached quota for {host}");
    }
    wait_until(lim_import);
    wait_until_key(lim_queries, &config.server);
    import(&config.server, &config.auth.token, status).context("import error ")?;
    Ok(())
}

// This wouldn't be needed if using async
// TODO: as a trait, maybe
fn wait_until_key<K>(lim: &governor::DefaultKeyedRateLimiter<K>, key: &K)
where
    K: Clone + Hash + Eq,
{
    while let Err(e) = lim.check_key(key) {
        sleep(e.wait_time_from(Instant::now()));
    }
}
// TODO: as a trait, maybe
fn wait_until(lim: &governor::DefaultDirectRateLimiter) {
    while let Err(e) = lim.check() {
        sleep(e.wait_time_from(Instant::now()));
    }
}

fn token<S: AsRef<str>>(server: S, client_id: S, client_secret: S, code: S) -> Result<String> {
    let response = client()?
        .post(format!("https://{}/oauth/token", server.as_ref()))
        .json(&TokenQuery {
            redirect_uri: OOB_URI,
            grant_type: GrantType::AuthorizationCode,
            code: Some(code.as_ref()),
            client_id: client_id.as_ref(),
            client_secret: client_secret.as_ref(),
            scope: Some(Scope::Read),
        })
        .send()
        .context("token post failed")?
        .with_error_text()?;
    let token: Token = response.json().context("token body not valid json")?;
    Ok(token.access_token)
}

fn hashtags(
    server: &str,
    token: &str,
    name: &str,
    any: &Option<Vec<String>>,
    limit: u8,
) -> Result<Vec<Status>> {
    let response: Vec<Status> = client()?
        .get(
            reqwest::Url::parse_with_params(
                &format!("https://{server}/api/v1/timelines/tag/{name}?limit={limit}"),
                //"any[]=kr2023&any[]=KernelRecipes2023",
                any.iter()
                    .flat_map(|l| l.iter().map(|h| ("any[]", h)))
                    .collect::<Vec<_>>(),
            )
            .with_context(|| format!("hashtags url for {server}"))?
            .as_str(),
        )
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .context("hashtags get failed")?
        .with_error_text()?
        .json()
        .context("hash tag statuses body not valid json")?;
    Ok(response)
}

fn import(server: &str, token: &str, url: &str) -> Result<()> {
    client()?
        .get(
            reqwest::Url::parse_with_params(
                &format!("https://{server}/api/v2/search"),
                &[
                    ("q", url),
                    ("resolve", "true"),
                    ("limit", "25"),
                    ("type", "statuses"),
                ],
            )
            .with_context(|| format!("import search url for {url}"))?
            .as_str(),
        )
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .context("import get failed")?
        .with_error_text()?;
    Ok(())
}

trait WithErrorText {
    fn with_error_text(self) -> Result<Self>
    where
        Self: Sized;
}
impl WithErrorText for reqwest::blocking::Response {
    fn with_error_text(self) -> Result<Self> {
        let status_err = self.error_for_status_ref();
        if let Err(e) = status_err {
            bail!(
                "Got response {}: {e}",
                self.text()
                    .with_context(|| format!("Got {e} and cannot read body"))?
            );
        }
        Ok(self)
    }
}
