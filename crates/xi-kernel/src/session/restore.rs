//! Construction and durable lineage restoration.

use super::*;

impl Session {
    /// Creates a timestamped log and broadcasts its root.
    ///
    /// Requires an entered Tokio runtime to start the session actor.
    pub fn create(
        config: Config,
        directory: Option<&Path>,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
    ) -> Result<Self> {
        Self::create_linked(config, directory, bus, options, backend, None)
    }

    pub(super) fn create_linked(
        config: Config,
        directory: Option<&Path>,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
        parent: Option<(SessionId, watch::Sender<snapshot::Snapshot>)>,
    ) -> Result<Self> {
        let directory = directory.map_or_else(|| config.session_dir(), Path::to_owned);
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let id = SessionId::generate();
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| Error::Config(e.to_string()))?
            .replace(':', "-");
        let path = directory.join(format!("{timestamp}_{id}.jsonl"));
        let log = SessionLog::create(
            path,
            EventKind::SessionStarted {
                session: id,
                cwd: config.workspace.clone(),
                model: config.model.clone(),
                xi_version: env!("CARGO_PKG_VERSION").into(),
                parent_session: parent.as_ref().map(|(id, _)| *id),
            },
        )?;
        let root = log.root().clone();
        let session = Self::restore(
            log,
            config,
            bus,
            options,
            backend,
            parent.map(|(_, snapshot)| snapshot),
        )?;
        session.publish_event(EventParams {
            session: id,
            event: root,
        });
        Ok(session)
    }

    /// Resumes at the latest leaf of an already opened log.
    ///
    /// Requires an entered Tokio runtime to start the session actor.
    pub fn from_log(
        log: SessionLog,
        config: Config,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
    ) -> Result<Self> {
        Self::restore(log, config, bus, options, backend, None)
    }

    fn restore(
        log: SessionLog,
        mut config: Config,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
        parent_snapshot: Option<watch::Sender<snapshot::Snapshot>>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Operation("session construction requires a Tokio runtime".into())
        })?;
        let EventKind::SessionStarted {
            session: id,
            cwd,
            model,
            ..
        } = &log.root().kind
        else {
            unreachable!("validated session root")
        };
        if cwd.canonicalize()? != config.workspace {
            return Err(Error::Config(
                "session workspace does not match configuration".into(),
            ));
        }
        let id = *id;
        let head = log.leaves().last().expect("session has a leaf").id;
        config.model = model.clone();
        let initial_reasoning = config.reasoning_override.unwrap_or(config.reasoning);
        config.reasoning = initial_reasoning;
        // Only an explicit invocation override beats saved reasoning. Config
        // defaults fill logs that have no reasoning change on this lineage.
        let restore_reasoning = config.reasoning_override.is_none();
        let mut pending = Vec::<(ToolCallId, bool)>::new();
        let mut asks = Vec::<ToolCallId>::new();
        for event in log.lineage(head).expect("known head") {
            match &event.kind {
                EventKind::ModelChanged { model } => config.model = model.clone(),
                EventKind::ReasoningChanged { reasoning } if restore_reasoning => {
                    config.reasoning = *reasoning;
                }
                EventKind::AssistantMessage { content, .. } => {
                    for block in content {
                        if let Block::ToolUse { id, .. } = block {
                            pending.push((id.clone(), false));
                        }
                    }
                }
                EventKind::ToolDecision { call, decision, .. } => {
                    if let Some((_, unresolved)) = pending.iter_mut().find(|(id, _)| id == call) {
                        *unresolved = *decision == Decision::Ask;
                    } else if *decision == Decision::Ask {
                        // An egress ask: it has no tool result of its own.
                        asks.push(call.clone());
                    }
                }
                EventKind::ApprovalResolved { call, .. } => {
                    if let Some((_, unresolved)) = pending.iter_mut().find(|(id, _)| id == call) {
                        *unresolved = false;
                    }
                    asks.retain(|id| id != call);
                }
                EventKind::ToolResult { call, .. } => pending.retain(|(id, _)| id != call),
                _ => {}
            }
        }
        let grants = lineage_grants(&log, head);
        let system = SystemContext::at(&config, log.root().at)?;
        let spill = log.file_path().with_extension("spill");
        let tmp = log.file_path().with_extension("tmp");
        std::fs::create_dir_all(&spill)?;
        std::fs::create_dir_all(&tmp)?;
        let spill = spill.canonicalize()?;
        let tmp = tmp.canonicalize()?;
        let git = log.file_path().with_extension("git");
        sandbox::write_sandbox_git(
            &git,
            &config.paths,
            config.sandbox.ssh_agent,
            config.host_probe(),
        )?;
        let git = git.canonicalize()?;
        let mut sandbox = config.sandbox_spec(&tmp, &spill, &system.skill_roots, &git)?;
        let listener = if sandbox.enabled {
            let listener = egress::Listener::bind("tools")?;
            sandbox.egress = Some(listener.egress().clone());
            Some(listener)
        } else {
            None
        };
        let (egress_tx, egress_rx) = mpsc::unbounded_channel();
        let executable = std::env::current_exe()?;
        let binary_dir = executable
            .parent()
            .ok_or_else(|| Error::Operation("executable has no parent directory".into()))?;
        let binary_dir = if binary_dir.file_name().is_some_and(|name| name == "deps") {
            binary_dir
                .parent()
                .ok_or_else(|| Error::Operation("Cargo target has no parent directory".into()))?
        } else {
            binary_dir
        };
        let helper = std::env::var_os("XI_SANDBOX_EXEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| binary_dir.join("xi-sandbox-exec"));
        let tools_context = xi_tools::Context {
            workspace: config.workspace.clone(),
            sandbox,
            helper,
            spill_dir: spill,
            output_limit: 32 * 1024,
            cancel: CancellationToken::new(),
        };
        let backend = backend.map(Into::into);
        let titles = title::Titles::from_events(log.events());
        let skills = system.skills.clone();

        let mut state = State {
            log,
            head,
            config,
            initial_reasoning,
            system,
            tools_context,
            backend,
            extensions: None,
            grants,
            egress: EgressState {
                listener,
                tx: egress_tx,
                rx: egress_rx,
            },
            titles,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let (snapshot, _) = watch::channel(snapshot::Snapshot::new(id, &state));
        let (running, _) = watch::channel(false);
        let (commands, _) = watch::channel(Vec::new());
        let (published_config, _) = watch::channel(state.config.clone());
        let session = Self(Arc::new(Inner {
            id,
            tx,
            snapshot,
            parent_snapshot,
            running,
            commands,
            config: published_config,
            skills,
            bus,
            extensions: extensions::ExtensionHost::new(id),
            options,
            stop: CancellationToken::new(),
            sequence: AtomicU64::new(0),
            child_sequence: AtomicU64::new(0),
            child_ready: watch::channel(0).0,
        }));
        for call in asks {
            session.append(
                &mut state,
                EventKind::ApprovalResolved {
                    call,
                    approved: false,
                    by: Approver::Interrupted,
                    remember: Remember::Once,
                },
            )?;
        }
        for (call, unresolved) in pending {
            if unresolved {
                session.append(
                    &mut state,
                    EventKind::ApprovalResolved {
                        call: call.clone(),
                        approved: false,
                        by: Approver::Interrupted,
                        remember: Remember::Once,
                    },
                )?;
            }
            session.append(
                &mut state,
                EventKind::ToolResult {
                    call,
                    output: error_output("interrupted"),
                    spill: None,
                },
            )?;
        }

        runtime.spawn(actor::run(Arc::downgrade(&session.0), Some(state), rx));
        Ok(session)
    }

    pub(super) fn checkout_state(&self, state: &mut State, event: EventId) -> Result<()> {
        let lineage = state.log.lineage(event).ok_or(Error::UnknownEvent(event))?;
        let mut selected = None;
        let mut reasoning = state.initial_reasoning;
        for event in lineage {
            match &event.kind {
                EventKind::SessionStarted { model, .. } | EventKind::ModelChanged { model } => {
                    selected = Some(model.clone())
                }
                EventKind::ReasoningChanged {
                    reasoning: selected,
                } => reasoning = *selected,
                _ => {}
            }
        }
        state.grants = lineage_grants(&state.log, event);
        state.head = event;
        state.config.reasoning = reasoning;
        if let Some(model) = selected
            && model != state.config.model
        {
            state.config.model = model;
            state.backend = None;
        }

        self.refresh(state);
        Ok(())
    }
}

fn lineage_grants(log: &SessionLog, head: EventId) -> HashMap<String, HashSet<ActionKey>> {
    let mut tools: HashMap<&ToolCallId, &String> = HashMap::new();
    let mut actions = HashMap::new();
    let mut grants: HashMap<String, HashSet<ActionKey>> = HashMap::new();
    for event in log.lineage(head).expect("known head") {
        match &event.kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    if let Block::ToolUse { id, name, .. } = block {
                        tools.insert(id, name);
                    }
                }
            }
            EventKind::ToolDecision {
                call,
                action,
                during,
                extension,
                ..
            } => {
                actions.insert(call, action);
                // An egress decision is keyed by the tool or extension that
                // connected, like the grant it may create.
                if let Some(name) = during
                    .as_ref()
                    .and_then(|during| tools.get(during).copied())
                    .or(extension.as_ref())
                {
                    tools.insert(call, name);
                }
            }
            EventKind::ApprovalResolved {
                call,
                approved: true,
                remember: Remember::Session | Remember::Workspace,
                ..
            } => {
                if let (Some(tool), Some(action)) = (tools.get(call), actions.get(call))
                    && let Some(action) = ActionKey::parse(action)
                {
                    grants.entry((*tool).clone()).or_default().insert(action);
                }
            }
            _ => {}
        }
    }
    grants
}
