//! Harness account logins: Codex, Claude Code, and Cursor, and resuming work a plan limit stopped (ADR 0117).
use super::*;
use crate::runtime::RESUME_WITHIN_MS;
use std::time::{SystemTime, UNIX_EPOCH};

/// Resumes in a row a plan limit gets before the user must step in (provider-retry's rule).
pub(super) const MAX_RESUMES: u32 = 4;
const RESUME_CHECK: Duration = Duration::from_secs(10);
/// A limit Core will not resume at its reset (over 6 hours away, unknown, or 4 resumes used) still gets a loop,
/// saved paused, so only an account switch runs it and wakes the sandbox (web/lib/automations.ts
/// `resumeAfterSwitch`). Its time is a placeholder.
const PAUSED_SECS: u64 = 30 * 24 * 60 * 60;

/// A plan-limit error that promised a resume time Core will not keep (4 resumes used): point to a switch instead.
pub(super) fn unpromise(mut error: String) -> String {
    if let Some(start) = error.find("Resumes at ")
        && let Some(end) = error[start..].find("accounts.")
    {
        error.replace_range(
            start..start + end + "accounts.".len(),
            "Switch accounts to keep working.",
        );
    }
    error
}

/// A prompt from a resume loop: the website opens it with the loop's name (web/lib/automations.ts `promptText`).
fn resume_prompt(input: &Value) -> bool {
    input["text"].as_str().is_some_and(|t| {
        t.starts_with("[Cloudroom automation \"Resume after ") && t.contains("plan limit · ")
    })
}

/// Whether `request` is a run of loop `id`: the website names each run `auto_<id without dashes>_<due>`.
pub(super) fn loop_request(id: &str, request: &str) -> bool {
    request.starts_with(&format!("auto_{}_", id.replace('-', "")))
}

/// What this session's resume loop needs on the website.
enum Change {
    /// At this reset, or paused (`None`).
    Create(Option<u64>),
    Delete(String),
}

impl Session {
    /// A resume prompt runs only from this limit's own loop, while the limit still stops the thread and no newer work
    /// waits. One from a retired or unknown loop, or sent after the thread moved on or was stopped, is refused.
    pub(super) fn resume_accepts(&self, request: &str, input: &Value) -> bool {
        let from_loop = resume_prompt(input)
            || self
                .resume_loop
                .iter()
                .chain(&self.resume_spent)
                .any(|id| loop_request(id, request));
        !from_loop
            || (self.usage_limited
                && !self.queue_paused
                && self.queue.is_empty()
                && self
                    .resume_loop
                    .as_deref()
                    .is_some_and(|id| loop_request(id, request)))
    }

    fn resume_change(&self, now: u64) -> Option<Change> {
        if let Some(id) = self.resume_spent.first() {
            return Some(Change::Delete(id.clone()));
        }
        let stopped = self.usage_limited
            && !self.queue_paused
            && !self.archived
            && self.close_request.is_none()
            && self.state != "closed";
        match &self.resume_loop {
            Some(id) => (!stopped).then(|| Change::Delete(id.clone())),
            None if stopped && !self.resume_refused => {
                Some(Change::Create(self.usage_resets_ms.filter(|at| {
                    self.limit_resumes < MAX_RESUMES && at.saturating_sub(now) <= RESUME_WITHIN_MS
                })))
            }
            None => None,
        }
    }

    /// Names the whole session, so two sessions never share a loop, and the provider, so the website can run a
    /// provider's loops on an account switch.
    fn resume_name(&self) -> String {
        let provider = match self.harness {
            runtime::Kind::Claude => "Claude ",
            runtime::Kind::Codex => "Codex ",
            _ => "",
        };
        format!("Resume after {provider}plan limit · {}", self.session_id)
    }
}

impl Manager {
    pub async fn claude_auth_status(&self) -> runtime::auth::Status {
        self.claude_login.status(&self.config).await
    }

    pub async fn claude_account(
        &self,
        action: &str,
        id: String,
        code: Option<String>,
        state: Option<String>,
    ) -> Result<runtime::auth::Status> {
        if self.is_stopping() {
            return Err(Error::Conflict("service is stopping"));
        }
        Ok(self
            .claude_login
            .action(&self.config, action, id, code, state)
            .await)
    }

    pub fn start_auth_monitor(&self) {
        self.claude_auth.start(&self.config, &self.observability);
    }

    pub async fn codex_auth_status(self: &Arc<Self>) -> runtime::auth::Status {
        let status = self.codex_auth.status(&self.config).await;
        if self.codex_auth.take_switched().await {
            self.continue_limited(&[runtime::Kind::Codex, runtime::Kind::Pi]);
        }
        status
    }

    /// Verify a login installed outside the core, then continue quota-stopped work.
    pub async fn codex_switched(self: &Arc<Self>) -> (runtime::auth::Status, Vec<String>) {
        let status = self.codex_auth.verify(&self.config).await;
        let continued = if status.state == "connected" {
            self.continue_limited(&[runtime::Kind::Codex, runtime::Kind::Pi])
        } else {
            Vec::new()
        };
        (status, continued)
    }

    /// The website wrote a new Claude login (ADR 0197): continue Claude work its plan limit stopped.
    pub fn claude_switched(self: &Arc<Self>) -> Vec<String> {
        self.continue_limited(&[runtime::Kind::Claude])
    }

    /// Send "keep working" to these harnesses' sessions stopped by a usage limit.
    /// The harness restarts or wakes first, so it loads the new login before the prompt runs.
    fn continue_limited(self: &Arc<Self>, harnesses: &[runtime::Kind]) -> Vec<String> {
        if self.is_stopping() || self.storage.blocks() {
            return Vec::new();
        }
        let mut local = self.local.lock().unwrap();
        let ids: Vec<_> = local
            .sessions
            .values()
            .filter(|s| {
                s.usage_limited
                    && !s.archived
                    && harnesses.contains(&s.harness)
                    // Awake and idle, going to sleep (the app's switch asks for it), or asleep.
                    && ((s.ready && s.handle.is_some() && s.state == "idle")
                        || s.releasing
                        || (s.handle.is_none() && s.state == "sleeping"))
                    && s.current_request.is_none()
                    && s.queue.is_empty()
                    && !s.queue_paused
                    && !s.compacting
                    && s.close_request.is_none()
                    && s.rewind_request.is_none()
            })
            .map(|s| s.session_id.clone())
            .collect();
        let mut continued = Vec::new();
        for id in ids {
            let receipt = Receipt {
                request_id: format!("continue_{}", now_ms()),
                command: "prompt".into(),
                input: json!({"text":"keep working"}),
                state: "accepted".into(),
                model: None,
                provider: None,
                workspace: None,
                error: None,
            };
            let Ok(data) = serde_json::to_value(&receipt) else {
                continue;
            };
            if local.append(&id, "receipt", data, None).is_err() {
                continue;
            }
            if let Some(handle) = &local.sessions[&id].handle {
                handle.request_shutdown();
            } else if self.wake(&mut local, &id).is_err() {
                continue;
            }
            continued.push(id);
        }
        continued
    }

    /// Cloud only: a thread a plan limit stopped gets one website loop that sends "keep working" just after the
    /// reset, since its sandbox is asleep by then (ADRs 0212, 0225). Once the thread moves on (a switch, the user's
    /// message, the loop itself), the loop is removed, so it never fires twice.
    pub fn start_limit_resumer(self: &Arc<Self>) {
        if self.history.api.is_none() {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            while !manager.is_stopping() {
                manager.sync_resume_loops().await;
                tokio::time::sleep(RESUME_CHECK).await;
            }
        });
    }

    async fn sync_resume_loops(&self) {
        let Some(api) = &self.history.api else {
            return;
        };
        let now = now_ms();
        let changes: Vec<_> = self
            .local
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter_map(|s| {
                let change = s.resume_change(now)?;
                Some((
                    s.session_id.clone(),
                    s.resume_name(),
                    s.limit_episode,
                    change,
                ))
            })
            .collect();
        // A refused change (a deleted loop, an archived thread, 100 loops) stays refused; an outage does not.
        use super::history::ApiError::Status;
        for (session, name, episode, change) in changes {
            let data = match change {
                Change::Delete(id) => match api
                    .post("automation", &json!({"action":"delete","id":id}))
                    .await
                {
                    Ok(_) => json!({"deleted":id}),
                    Err(Status(status, _)) if status < 500 => json!({"deleted":id}),
                    Err(_) => continue,
                },
                Change::Create(at) => {
                    // A create whose answer was lost, or a restart, may have left this session a loop: remove it
                    // first, since its schedule may be an earlier limit's, so there is never a second one.
                    let Ok(list) = api.post("automation", &json!({"action":"list"})).await else {
                        continue;
                    };
                    let mut cleared = true;
                    for l in list["automations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|l| {
                            l["name"] == name.as_str()
                                && (l["session_id"].is_null()
                                    || l["session_id"] == session.as_str())
                        })
                    {
                        cleared &= match api
                            .post("automation", &json!({"action":"delete","id":l["id"]}))
                            .await
                        {
                            Ok(_) => true,
                            Err(Status(status, _)) => status < 500,
                            Err(_) => false,
                        };
                    }
                    if !cleared {
                        continue;
                    }
                    // 1-3 minutes after the reset, so threads on one account do not all retry at once.
                    let jitter = u64::from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_millis(),
                    ) * 120;
                    let delay = at.map_or(PAUSED_SECS, |at| {
                        at.saturating_sub(now) / 1000 + 60 + jitter / 1000
                    });
                    let body = json!({"action":"create","name":name,"prompt":"keep working","delay":format!("{delay} seconds"),"new_thread":false,"session":session});
                    match api.post("automation", &body).await {
                        Ok(answer) => match answer["id"].as_str() {
                            Some(id) => {
                                if at.is_none() {
                                    // Left unpaused by a lost answer, it still waits a month first.
                                    let _ = api
                                        .post("automation", &json!({"action":"pause","id":id}))
                                        .await;
                                }
                                json!({"id":id,"episode":episode})
                            }
                            None => {
                                json!({"id":null,"failed":"the website returned no loop","episode":episode})
                            }
                        },
                        Err(Status(status, message)) if status < 500 => {
                            json!({"id":null,"failed":message,"episode":episode})
                        }
                        Err(_) => continue,
                    }
                }
            };
            let _ = self
                .local
                .lock()
                .unwrap()
                .append(&session, "resume_loop", data, None);
        }
    }

    fn check_codex_login(&self) -> Result<()> {
        if self.is_stopping() {
            return Err(Error::Conflict("service is stopping"));
        }
        if self.storage.blocks() {
            return Err(Error::Conflict("storage unsafe; new execution is blocked"));
        }
        if self.local.lock().unwrap().sessions.values().any(|s| {
            s.harness == runtime::Kind::Codex
                && (s.current_request.is_some()
                    || (!s.queue_paused && !s.queue.is_empty())
                    || matches!(s.state.as_str(), "starting" | "resuming" | "pending"))
        }) {
            return Err(Error::Conflict(
                "finish active Codex work before signing in",
            ));
        }
        Ok(())
    }

    pub async fn codex_login(&self, request: String) -> Result<runtime::auth::Status> {
        self.check_codex_login()?;
        Ok(self.codex_auth.login(&self.config, request).await)
    }

    pub async fn import_codex_login(
        self: &Arc<Self>,
        credentials: Value,
    ) -> Result<runtime::auth::Status> {
        self.check_codex_login()?;
        let status = self
            .codex_auth
            .import(&self.config, &self.storage, credentials)
            .await;
        if self.codex_auth.take_switched().await {
            self.continue_limited(&[runtime::Kind::Codex, runtime::Kind::Pi]);
        }
        Ok(status)
    }

    pub async fn cancel_codex_login(&self, request: &str) -> runtime::auth::Status {
        self.codex_auth.cancel(request).await
    }

    pub async fn cursor_auth_status(&self) -> runtime::auth::Status {
        self.cursor_auth.status(&self.config, &self.storage).await
    }
    pub async fn cursor_account(
        &self,
        action: &str,
        request: String,
        key: Option<String>,
    ) -> Result<runtime::auth::Status> {
        if action == "cancel" {
            return Ok(self.cursor_auth.cancel(&request).await);
        }
        if self.is_stopping() {
            return Err(Error::Conflict("service is stopping"));
        }
        if self.storage.blocks() {
            return Err(Error::Conflict("storage unsafe; new execution is blocked"));
        }
        if self.local.lock().unwrap().sessions.values().any(|session| {
            session.harness == runtime::Kind::Cursor
                && (session.handle.is_some()
                    || session.current_request.is_some()
                    || (!session.queue_paused && !session.queue.is_empty())
                    || matches!(session.state.as_str(), "starting" | "resuming" | "pending"))
        }) {
            return Err(Error::Conflict(
                "close Cursor sessions before changing the cloud login",
            ));
        }
        Ok(if let Some(key) = key {
            self.cursor_auth
                .set_key(&self.config, &self.storage, key)
                .await
        } else {
            self.cursor_auth
                .login(&self.config, &self.storage, request)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;

    /// A journal with one idle Codex session, and a helper that accepts a prompt for it.
    fn limited_session(dir: &str) -> (Local, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("cloudroom-resume-{dir}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut local = Local::open(&dir).unwrap();
        let start = json!({"request_id":"s1","command":"start","state":"accepted","input":{"harness":"codex"}});
        local.append("a", "receipt", start, None).unwrap();
        local
            .append("a", "state", json!({"state":"idle"}), None)
            .unwrap();
        (local, dir)
    }

    fn prompt(local: &mut Local, request: &str) {
        let receipt = json!({"request_id":request,"command":"prompt","state":"accepted","input":{"text":"keep working"}});
        local.append("a", "receipt", receipt, None).unwrap();
    }

    fn change(local: &Local, now: u64) -> String {
        match local.sessions["a"].resume_change(now) {
            Some(Change::Create(Some(_))) => "create timed".into(),
            Some(Change::Create(None)) => "create paused".into(),
            Some(Change::Delete(id)) => format!("delete {id}"),
            None => "none".into(),
        }
    }

    #[test]
    fn sessions_with_the_same_ending_get_their_own_loops() {
        let named = |id: &str| Session {
            session_id: id.into(),
            harness: runtime::Kind::Claude,
            ..Session::default()
        };
        let first = named("cr_auto_00000000000000000000000000000001_1792500000").resume_name();
        let second = named("cr_auto_00000000000000000000000000000002_1792500000").resume_name();
        assert_ne!(first, second);
        assert!(first.starts_with("Resume after Claude plan limit · "));
    }

    #[test]
    fn a_new_limit_after_a_continuation_replaces_the_old_schedule() {
        let (mut local, dir) = limited_session("episode");
        let now = now_ms();
        // Enabled loop for a reset in 2 minutes; a switch continues the thread; the next turn's reset is 7 h away.
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        assert_eq!(change(&local, now), "create timed");
        let episode = local.sessions["a"].limit_episode;
        local
            .append(
                "a",
                "resume_loop",
                json!({"id":"timed-1","episode":episode}),
                None,
            )
            .unwrap();
        prompt(&mut local, "continue_1");
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 7 * HOUR}),
                None,
            )
            .unwrap();
        // The old loop goes first, before any poll sees the thread unlimited; then a paused loop for the new limit.
        assert_eq!(change(&local, now), "delete timed-1");
        local
            .append("a", "resume_loop", json!({"deleted":"timed-1"}), None)
            .unwrap();
        assert_eq!(change(&local, now), "create paused");
        let episode = local.sessions["a"].limit_episode;
        local
            .append(
                "a",
                "resume_loop",
                json!({"id":"paused-1","episode":episode}),
                None,
            )
            .unwrap();
        // And back: a user retry hits a limit that resets soon, so the paused loop gives way to a timed one.
        prompt(&mut local, "user_1");
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        assert_eq!(change(&local, now), "delete paused-1");
        local
            .append("a", "resume_loop", json!({"deleted":"paused-1"}), None)
            .unwrap();
        assert_eq!(change(&local, now), "create timed");
        assert_eq!(local.sessions["a"].limit_resumes, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_late_answer_for_an_earlier_limit_is_retired() {
        let (mut local, dir) = limited_session("late");
        let now = now_ms();
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        let earlier = local.sessions["a"].limit_episode;
        // Before the website answers, the thread moves on and hits another limit.
        prompt(&mut local, "user_1");
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        local
            .append(
                "a",
                "resume_loop",
                json!({"id":"late-1","episode":earlier}),
                None,
            )
            .unwrap();
        assert_eq!(local.sessions["a"].resume_loop, None);
        assert_eq!(change(&local, now), "delete late-1");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_limit_on_a_turn_with_newer_work_queued_behind_it_ends_when_that_work_starts() {
        let (mut local, dir) = limited_session("queued");
        let now = now_ms();
        let run = |local: &mut Local, request: &str, state: &str| {
            local
                .append(
                    "a",
                    "state",
                    json!({"state":state,"request_id":request}),
                    None,
                )
                .unwrap();
        };
        let finish = |local: &mut Local, request: &str, state: &str| {
            let receipt =
                json!({"request_id":request,"command":"prompt","state":state,"input":{"text":"x"}});
            local.append("a", "receipt", receipt, None).unwrap();
        };
        // A runs; B is accepted behind it; then A hits the limit, so the limit is recorded after B's acceptance.
        prompt(&mut local, "a_task");
        run(&mut local, "a_task", "starting_turn");
        prompt(&mut local, "b_task");
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        finish(&mut local, "a_task", "failed");
        // While B waits in the queue, a loop's run may not slip in behind it.
        let episode = local.sessions["a"].limit_episode;
        local
            .append(
                "a",
                "resume_loop",
                json!({"id":"aaaa-1111","episode":episode}),
                None,
            )
            .unwrap();
        let resume = json!({"text":"[Cloudroom automation \"Resume after Codex plan limit · a\" due: x]\n\nkeep working"});
        assert!(!local.sessions["a"].resume_accepts("auto_aaaa1111_1", &resume));
        // B starts: A's limit no longer stops the thread, and its loop retires while B runs and after it completes.
        run(&mut local, "b_task", "starting_turn");
        assert!(!local.sessions["a"].usage_limited);
        assert!(!local.sessions["a"].resume_accepts("auto_aaaa1111_1", &resume));
        assert_eq!(change(&local, now), "delete aaaa-1111");
        local
            .append("a", "resume_loop", json!({"deleted":"aaaa-1111"}), None)
            .unwrap();
        finish(&mut local, "b_task", "completed");
        run(&mut local, "", "idle");
        assert_eq!(change(&local, now), "none");
        assert!(!local.sessions["a"].resume_accepts("auto_aaaa1111_1", &resume));
        // A real limit on later work still gets its own loop.
        prompt(&mut local, "c_task");
        run(&mut local, "c_task", "starting_turn");
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        finish(&mut local, "c_task", "failed");
        assert_eq!(change(&local, now), "create timed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_current_loop_may_resume_the_thread() {
        let (mut local, dir) = limited_session("guard");
        let now = now_ms();
        let text = |id: &str| json!({"text": format!("[Cloudroom automation \"Resume after Codex plan limit · a\" due: {id}]\n\nkeep working")});
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        let episode = local.sessions["a"].limit_episode;
        local
            .append(
                "a",
                "resume_loop",
                json!({"id":"aaaa-1111","episode":episode}),
                None,
            )
            .unwrap();
        let s = &local.sessions["a"];
        assert!(s.resume_accepts("auto_aaaa1111_1792500000", &text("aaaa-1111")));
        assert!(
            !s.resume_accepts("auto_bbbb2222_1792500000", &text("bbbb-2222")),
            "another loop's run"
        );
        assert!(
            s.resume_accepts("user_1", &json!({"text":"hello"})),
            "an ordinary prompt"
        );
        // The thread moves on: the loop is retired, and stays refused even once a new limit stops the thread.
        prompt(&mut local, "user_2");
        assert!(
            !local.sessions["a"].resume_accepts("auto_aaaa1111_1792500000", &text("aaaa-1111"))
        );
        local
            .append(
                "a",
                "usage_limited",
                json!({"resets_at_ms": now + 120_000}),
                None,
            )
            .unwrap();
        assert!(
            !local.sessions["a"]
                .resume_accepts("auto_aaaa1111_1792500000", &json!({"text":"keep working"}))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
