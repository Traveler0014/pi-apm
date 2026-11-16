//! Shared test fixture. The clock is always injected, never read from the wall
//! clock, so lease expiry and staleness are deterministic.

use apm::*;

pub struct Fx {
    pub store: Store,
    pub project: String,
    pub t0: i64,
}

impl Fx {
    pub fn new() -> Fx {
        let mut store = Store::open_in_memory().expect("store");
        let settings = Settings {
            lease_seconds: 60,
            stale_after_seconds: 10,
            no_progress_after_seconds: 3600,
            done_recent_seconds: 86_400,
        };
        let t0 = 1_700_000_000_000;
        let (project, _) = store
            .init_project(
                "Test",
                Some("TST"),
                Some("/tmp/test"),
                settings,
                &Actor::human("setup"),
            )
            .expect("init");
        Fx {
            store,
            project: project.id,
            t0,
        }
    }

    pub fn at(&self, offset_secs: i64) -> i64 {
        self.t0 + offset_secs * 1000
    }

    pub fn agent(&self, name: &str, session: &str) -> Actor {
        Actor::agent(name).with_session(Some(session.to_string()))
    }

    pub fn human(&self, name: &str) -> Actor {
        Actor::human(name)
    }

    pub fn doc(&self, goal: &str, state: &str) -> ContextDoc {
        ContextDoc {
            goal: goal.to_string(),
            current_state: state.to_string(),
            constraints: vec!["no server".into()],
            references: vec!["src/lib.rs".into()],
            notes: None,
        }
    }

    pub fn create(&mut self, title: &str) -> WorkItemView {
        let actor = self.agent("agent-a", "sess-a");
        self.store
            .create_item(
                Some(&self.project),
                NewItem {
                    title: title.to_string(),
                    context: self.doc(&format!("finish {title}"), "not started"),
                    item_type: None,
                    priority: None,
                    parent: None,
                    depends_on: vec![],
                    labels: vec![],
                    acceptance: vec![],
                    actor,
                },
                self.t0,
                None,
            )
            .expect("create")
            .item
    }

    pub fn claim(&mut self, identifier: &str, who: &Actor, offset: i64) -> (ClaimView, String) {
        let issued = self
            .store
            .claim(
                identifier,
                who,
                ClaimRequest::default(),
                self.at(offset),
                None,
            )
            .expect("claim");
        (issued.claim, issued.token)
    }

    pub fn progress(
        &mut self,
        identifier: &str,
        token: &str,
        who: &Actor,
        offset: i64,
        summary: &str,
    ) -> ProgressOutcome {
        let version = self
            .store
            .get_item(identifier, self.at(offset))
            .unwrap()
            .item
            .version;
        self.store
            .record_progress(
                identifier,
                Some(token),
                Some(version),
                summary,
                Some(Some("next step".into())),
                None,
                None,
                None,
                who,
                self.at(offset),
                None,
            )
            .expect("progress")
    }
}
