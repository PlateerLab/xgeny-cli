use std::env;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use xgeny_cli::{DriverProgress, DriverProgressControl, LocalCommandResult, PauseReason};

const MAX_REPL_LINE_BYTES: usize = 16 * 1024;
const MAX_REPL_GOAL_BYTES: usize = 16 * 1024;
const DEVELOPER_EXECUTABLES: &[&str] = &[
    "git",
    "cargo",
    "rustc",
    "node",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "bun",
    "python",
    "python3",
    "pytest",
    "uv",
    "go",
    "make",
    "cmake",
    "ninja",
    "dotnet",
    "java",
    "javac",
    "gradle",
    "mvn",
    "swift",
    "xcodebuild",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    Ask,
    Allow,
    Deny,
}

impl PermissionMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionKind {
    Model,
    Read,
    Write,
    Execute,
}

impl PermissionKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "model" | "egress" => Some(Self::Model),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "execute" | "process" => Some(Self::Execute),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PermissionSettings {
    model: PermissionMode,
    read: PermissionMode,
    write: PermissionMode,
    execute: PermissionMode,
}

impl Default for PermissionSettings {
    fn default() -> Self {
        Self {
            model: PermissionMode::Ask,
            read: PermissionMode::Ask,
            write: PermissionMode::Ask,
            execute: PermissionMode::Ask,
        }
    }
}

impl PermissionSettings {
    const fn get(self, kind: PermissionKind) -> PermissionMode {
        match kind {
            PermissionKind::Model => self.model,
            PermissionKind::Read => self.read,
            PermissionKind::Write => self.write,
            PermissionKind::Execute => self.execute,
        }
    }

    const fn set(&mut self, kind: PermissionKind, mode: PermissionMode) {
        match kind {
            PermissionKind::Model => self.model = mode,
            PermissionKind::Read => self.read = mode,
            PermissionKind::Write => self.write = mode,
            PermissionKind::Execute => self.execute = mode,
        }
    }

    const fn grants(self) -> InvocationGrants {
        InvocationGrants {
            model: matches!(self.model, PermissionMode::Allow),
            read: matches!(self.read, PermissionMode::Allow),
            write: matches!(self.write, PermissionMode::Allow),
            execute: matches!(self.execute, PermissionMode::Allow),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Four independent runtime approval gates.
pub(crate) struct InvocationGrants {
    pub(crate) model: bool,
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) execute: bool,
}

impl InvocationGrants {
    const fn with(mut self, kind: PermissionKind) -> Self {
        match kind {
            PermissionKind::Model => self.model = true,
            PermissionKind::Read => self.read = true,
            PermissionKind::Write => self.write = true,
            PermissionKind::Execute => self.execute = true,
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelView {
    pub(crate) profile: String,
    pub(crate) model: String,
    pub(crate) authentication: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplFailure {
    code: &'static str,
}

impl ReplFailure {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self { code }
    }

    const fn code(self) -> &'static str {
        self.code
    }
}

pub(crate) trait ReplHost {
    fn model(&mut self) -> Result<ModelView, ReplFailure>;

    fn use_model(&mut self, name: &str) -> Result<ModelView, ReplFailure>;

    fn executable_ids(&self) -> &[String];

    fn start(
        &mut self,
        goal: String,
        grants: InvocationGrants,
        progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
    ) -> Result<LocalCommandResult, ReplFailure>;

    fn resume(
        &mut self,
        run_id: &str,
        grants: InvocationGrants,
        progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
    ) -> Result<LocalCommandResult, ReplFailure>;
}

#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    requested: Arc<AtomicBool>,
}

impl Cancellation {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    fn clear(&self) {
        self.requested.store(false, Ordering::SeqCst);
    }
}

enum InputMessage {
    Line(String),
    TooLarge,
    Unavailable,
    EndOfInput,
}

/// Bounded stdin bridge that lets the REPL observe Ctrl+C even when the platform restarts a
/// blocking terminal read.
pub(crate) struct InterruptibleInput {
    receiver: Receiver<InputMessage>,
    cancellation: Cancellation,
    buffer: Vec<u8>,
    position: usize,
    ended: bool,
}

impl InterruptibleInput {
    pub(crate) fn stdin(cancellation: Cancellation) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            loop {
                let message = match read_bounded_line(&mut input) {
                    Ok(Some(line)) => InputMessage::Line(line),
                    Ok(None) => InputMessage::EndOfInput,
                    Err(InputFailure::TooLarge) => InputMessage::TooLarge,
                    Err(InputFailure::Unavailable | InputFailure::InvalidCommand) => {
                        InputMessage::Unavailable
                    }
                };
                let ended = matches!(
                    message,
                    InputMessage::EndOfInput | InputMessage::Unavailable
                );
                if sender.send(message).is_err() || ended {
                    return;
                }
            }
        });
        Self {
            receiver,
            cancellation,
            buffer: Vec::new(),
            position: 0,
            ended: false,
        }
    }

    fn receive(&mut self) -> io::Result<()> {
        while !self.ended && self.position == self.buffer.len() {
            if self.cancellation.is_requested() {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            match self.receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(InputMessage::Line(line)) => {
                    self.buffer = line.into_bytes();
                    self.buffer.push(b'\n');
                    self.position = 0;
                }
                Ok(InputMessage::TooLarge) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "interactive input exceeds its fixed bound",
                    ));
                }
                Ok(InputMessage::Unavailable) => {
                    return Err(io::Error::other("interactive stdin is unavailable"));
                }
                Ok(InputMessage::EndOfInput) | Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        Ok(())
    }
}

impl Read for InterruptibleInput {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(destination.len());
        destination[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for InterruptibleInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.receive()?;
        Ok(&self.buffer[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        self.position = self.position.saturating_add(amount).min(self.buffer.len());
        if self.position == self.buffer.len() {
            self.buffer.clear();
            self.position = 0;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplCommand {
    Model(Option<String>),
    Status,
    Permissions(Option<(PermissionKind, PermissionMode)>),
    Resume(Option<String>),
    Clear,
    Help,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplEntry {
    Empty,
    Goal(String),
    Command(ReplCommand),
    Interrupted,
    EndOfInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputFailure {
    Unavailable,
    TooLarge,
    InvalidCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    Idle,
    Completed,
    Paused,
    Rejected,
    RecoveryRequired,
}

impl SessionOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Completed => "completed",
            Self::Paused => "paused",
            Self::Rejected => "rejected",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run<R: BufRead, W: Write, H: ReplHost>(
    reader: &mut R,
    output: &mut W,
    host: &mut H,
    cancellation: &Cancellation,
) -> io::Result<()> {
    writeln!(output, "XGENy Developer Preview")?;
    writeln!(
        output,
        "Type a goal, or /help for commands. End a line with \\ for multiline input."
    )?;
    writeln!(
        output,
        "Local tools are catalogued; every model/read/write/execute boundary keeps its own approval."
    )?;

    let mut permissions = PermissionSettings::default();
    let mut active_run = None;
    let mut last_run = None;
    let mut previous_summary = None;
    let mut outcome = SessionOutcome::Idle;

    loop {
        let entry = match read_entry(reader, output) {
            Ok(entry) => entry,
            Err(InputFailure::TooLarge) => {
                writeln!(output, "error: input_too_large")?;
                continue;
            }
            Err(InputFailure::InvalidCommand) => {
                writeln!(output, "error: command_invalid")?;
                continue;
            }
            Err(InputFailure::Unavailable) => {
                return Err(io::Error::other("interactive input unavailable"));
            }
        };
        match entry {
            ReplEntry::Empty => {}
            ReplEntry::Interrupted => {
                cancellation.clear();
                writeln!(output, "^C")?;
            }
            ReplEntry::EndOfInput | ReplEntry::Command(ReplCommand::Exit) => {
                writeln!(output, "bye")?;
                return Ok(());
            }
            ReplEntry::Command(ReplCommand::Help) => print_help(output)?,
            ReplEntry::Command(ReplCommand::Model(name)) => match name {
                Some(name) => match host.use_model(&name) {
                    Ok(model) => print_model(output, &model)?,
                    Err(error) => print_failure(output, error)?,
                },
                None => match host.model() {
                    Ok(model) => print_model(output, &model)?,
                    Err(error) => print_failure(output, error)?,
                },
            },
            ReplEntry::Command(ReplCommand::Status) => {
                writeln!(output, "status: {}", outcome.label())?;
                writeln!(output, "workspace: current_directory")?;
                writeln!(
                    output,
                    "active_run: {}",
                    active_run.as_deref().unwrap_or("none")
                )?;
                writeln!(
                    output,
                    "last_run: {}",
                    last_run.as_deref().unwrap_or("none")
                )?;
                writeln!(
                    output,
                    "session_context: {}",
                    if previous_summary.is_some() {
                        "available"
                    } else {
                        "none"
                    }
                )?;
                writeln!(
                    output,
                    "catalogued_executables: {}",
                    host.executable_ids().len()
                )?;
                print_permissions(output, permissions)?;
            }
            ReplEntry::Command(ReplCommand::Permissions(update)) => {
                if let Some((kind, mode)) = update {
                    permissions.set(kind, mode);
                }
                print_permissions(output, permissions)?;
                if !host.executable_ids().is_empty() {
                    write!(output, "executables:")?;
                    for id in host.executable_ids() {
                        write!(output, " ")?;
                        write_terminal_text(output, id)?;
                    }
                    writeln!(output)?;
                }
            }
            ReplEntry::Command(ReplCommand::Clear) => {
                active_run = None;
                last_run = None;
                previous_summary = None;
                outcome = SessionOutcome::Idle;
                writeln!(output, "session cleared; durable Runs were not deleted")?;
            }
            ReplEntry::Command(ReplCommand::Resume(run_id)) => {
                let selected = run_id
                    .or_else(|| active_run.clone())
                    .or_else(|| last_run.clone());
                let Some(run_id) = selected else {
                    writeln!(output, "error: run_id_required")?;
                    continue;
                };
                let grants = permissions.grants();
                let result = invoke_with_progress(output, cancellation, |progress| {
                    host.resume(&run_id, grants, progress)
                })?;
                handle_result(
                    reader,
                    output,
                    host,
                    cancellation,
                    result,
                    permissions,
                    &mut active_run,
                    &mut last_run,
                    &mut previous_summary,
                    &mut outcome,
                )?;
            }
            ReplEntry::Goal(goal) => {
                if active_run.is_some() {
                    writeln!(output, "error: active_run_paused; use /resume or /clear")?;
                    continue;
                }
                let mut grants = permissions.grants();
                match permissions.model {
                    PermissionMode::Allow => {}
                    PermissionMode::Deny => {
                        writeln!(output, "paused: model_egress_denied")?;
                        continue;
                    }
                    PermissionMode::Ask => {
                        if !prompt_approval(reader, output, PermissionKind::Model)? {
                            writeln!(output, "cancelled before model egress")?;
                            continue;
                        }
                        grants = grants.with(PermissionKind::Model);
                    }
                }
                let contextual_goal = compose_session_goal(&goal, previous_summary.as_deref());
                let result = invoke_with_progress(output, cancellation, |progress| {
                    host.start(contextual_goal, grants, progress)
                })?;
                handle_result(
                    reader,
                    output,
                    host,
                    cancellation,
                    result,
                    permissions,
                    &mut active_run,
                    &mut last_run,
                    &mut previous_summary,
                    &mut outcome,
                )?;
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_result<R: BufRead, W: Write, H: ReplHost>(
    reader: &mut R,
    output: &mut W,
    host: &mut H,
    cancellation: &Cancellation,
    mut result: Result<LocalCommandResult, ReplFailure>,
    permissions: PermissionSettings,
    active_run: &mut Option<String>,
    last_run: &mut Option<String>,
    previous_summary: &mut Option<String>,
    outcome: &mut SessionOutcome,
) -> io::Result<()> {
    loop {
        match result {
            Err(error) => {
                print_failure(output, error)?;
                return Ok(());
            }
            Ok(LocalCommandResult::Completed { run_id, summary }) => {
                cancellation.clear();
                *active_run = None;
                *last_run = Some(run_id.clone());
                *previous_summary = Some(summary.clone());
                *outcome = SessionOutcome::Completed;
                writeln!(output, "completed: {run_id}")?;
                write_terminal_text(output, &summary)?;
                if !summary.ends_with('\n') {
                    writeln!(output)?;
                }
                return Ok(());
            }
            Ok(LocalCommandResult::Paused { run_id, reason }) => {
                if let Some(run_id) = run_id {
                    *active_run = Some(run_id.clone());
                    *last_run = Some(run_id);
                }
                *outcome = SessionOutcome::Paused;
                if reason == PauseReason::UserCancelled || cancellation.is_requested() {
                    cancellation.clear();
                    writeln!(output, "paused: user_cancelled")?;
                    return Ok(());
                }
                let Some(kind) = pause_permission(reason) else {
                    writeln!(output, "paused: {}", reason.code())?;
                    return Ok(());
                };
                let Some(run_id) = active_run.clone() else {
                    writeln!(output, "paused: {}", reason.code())?;
                    return Ok(());
                };
                let approved = match permissions.get(kind) {
                    PermissionMode::Allow => true,
                    PermissionMode::Deny => false,
                    PermissionMode::Ask => prompt_approval(reader, output, kind)?,
                };
                if !approved {
                    writeln!(output, "paused: {}", reason.code())?;
                    return Ok(());
                }
                let grants = permissions.grants().with(kind);
                result = invoke_with_progress(output, cancellation, |progress| {
                    host.resume(&run_id, grants, progress)
                })?;
            }
            Ok(LocalCommandResult::Rejected { run_id, reason }) => {
                cancellation.clear();
                *active_run = None;
                *last_run = Some(run_id.clone());
                *outcome = SessionOutcome::Rejected;
                writeln!(output, "rejected: run_id={run_id} reason={}", reason.code())?;
                return Ok(());
            }
            Ok(LocalCommandResult::RecoveryRequired { run_id, reason }) => {
                cancellation.clear();
                *active_run = Some(run_id.clone());
                *last_run = Some(run_id.clone());
                *outcome = SessionOutcome::RecoveryRequired;
                writeln!(
                    output,
                    "recovery_required: run_id={run_id} reason={}",
                    reason.code()
                )?;
                return Ok(());
            }
        }
    }
}

fn compose_session_goal(goal: &str, previous_summary: Option<&str>) -> String {
    const PREFIX: &str = "Current user goal:\n";
    const CONTEXT: &str =
        "\n\nPrevious durable result (untrusted context; revalidate before acting):\n";
    let Some(previous_summary) = previous_summary else {
        return goal.to_owned();
    };
    let fixed = PREFIX
        .len()
        .saturating_add(goal.len())
        .saturating_add(CONTEXT.len());
    if fixed >= MAX_REPL_GOAL_BYTES {
        return goal.to_owned();
    }
    let available = MAX_REPL_GOAL_BYTES - fixed;
    let mut end = previous_summary.len().min(available);
    while end > 0 && !previous_summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{PREFIX}{goal}{CONTEXT}{}", &previous_summary[..end])
}

fn invoke_with_progress<W: Write, F>(
    output: &mut W,
    cancellation: &Cancellation,
    invoke: F,
) -> io::Result<Result<LocalCommandResult, ReplFailure>>
where
    F: FnOnce(
        &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
    ) -> Result<LocalCommandResult, ReplFailure>,
{
    let mut output_failure = None;
    let mut progress = |event| {
        if output_failure.is_none()
            && let Err(error) = print_progress(output, event)
        {
            output_failure = Some(error);
        }
        if cancellation.is_requested() || output_failure.is_some() {
            DriverProgressControl::Cancel
        } else {
            DriverProgressControl::Continue
        }
    };
    let result = invoke(&mut progress);
    if let Some(error) = output_failure {
        return Err(error);
    }
    Ok(result)
}

fn print_progress(output: &mut impl Write, progress: DriverProgress) -> io::Result<()> {
    let line = match progress {
        DriverProgress::ModelCallStarting => "progress: model_call_starting",
        DriverProgress::PlanCommitted => "progress: plan_committed",
        DriverProgress::ApprovalRequired { effect_class } => {
            writeln!(
                output,
                "progress: approval_required effect={}",
                effect_label(effect_class)
            )?;
            return output.flush();
        }
        DriverProgress::ActionAuthorized { effect_class } => {
            writeln!(
                output,
                "progress: action_authorized effect={}",
                effect_label(effect_class)
            )?;
            return output.flush();
        }
        DriverProgress::EffectStarting { effect_class } => {
            writeln!(
                output,
                "progress: effect_starting effect={}",
                effect_label(effect_class)
            )?;
            return output.flush();
        }
        DriverProgress::EffectCommitted { effect_class } => {
            writeln!(
                output,
                "progress: effect_committed effect={}",
                effect_label(effect_class)
            )?;
            return output.flush();
        }
        DriverProgress::VerificationStarting => "progress: verification_starting",
        DriverProgress::VerificationCommitted => "progress: verification_committed",
        DriverProgress::CompletionCommitted => "progress: completion_committed",
    };
    writeln!(output, "{line}")?;
    output.flush()
}

const fn effect_label(effect_class: xgeny_domain::EffectClass) -> &'static str {
    match effect_class {
        xgeny_domain::EffectClass::ReadOnly => "read",
        xgeny_domain::EffectClass::Idempotent => "write",
        xgeny_domain::EffectClass::NonIdempotent => "execute",
        xgeny_domain::EffectClass::Compensatable => "compensatable",
        xgeny_domain::EffectClass::Unknown => "unknown",
    }
}

const fn pause_permission(reason: PauseReason) -> Option<PermissionKind> {
    match reason {
        PauseReason::RemoteModelEgressConsentRequired => Some(PermissionKind::Model),
        PauseReason::ReadApprovalRequired => Some(PermissionKind::Read),
        PauseReason::WriteApprovalRequired => Some(PermissionKind::Write),
        PauseReason::ExecuteApprovalRequired => Some(PermissionKind::Execute),
        PauseReason::UserCancelled | PauseReason::TickBudgetExhausted | PauseReason::Quiescent => {
            None
        }
    }
}

fn prompt_approval<R: BufRead, W: Write>(
    reader: &mut R,
    output: &mut W,
    kind: PermissionKind,
) -> io::Result<bool> {
    if kind == PermissionKind::Model {
        write!(
            output,
            "Allow sending the goal, session context, and tool observations to the model? [y/N] "
        )?;
    } else {
        write!(
            output,
            "Allow {} for this durable continuation? [y/N] ",
            kind.label()
        )?;
    }
    output.flush()?;
    match read_bounded_line(reader) {
        Ok(Some(value)) => Ok(matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        )),
        Ok(None) | Err(InputFailure::TooLarge | InputFailure::InvalidCommand) => Ok(false),
        Err(InputFailure::Unavailable) => Err(io::Error::other("approval input unavailable")),
    }
}

fn read_entry<R: BufRead, W: Write>(
    reader: &mut R,
    output: &mut W,
) -> Result<ReplEntry, InputFailure> {
    let mut lines = Vec::new();
    loop {
        write!(
            output,
            "{}",
            if lines.is_empty() { "xgeny> " } else { "...> " }
        )
        .map_err(|_| InputFailure::Unavailable)?;
        output.flush().map_err(|_| InputFailure::Unavailable)?;
        let Some(mut line) = read_bounded_line(reader)? else {
            return Ok(if lines.is_empty() {
                ReplEntry::EndOfInput
            } else {
                ReplEntry::Goal(lines.join("\n"))
            });
        };
        if line == "\u{3}" {
            return Ok(ReplEntry::Interrupted);
        }
        if has_continuation(&line) {
            line.pop();
            lines.push(line);
            if joined_size(&lines) > MAX_REPL_GOAL_BYTES {
                return Err(InputFailure::TooLarge);
            }
            continue;
        }
        if lines.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Ok(ReplEntry::Empty);
            }
            if trimmed.starts_with('/') {
                return parse_command(trimmed).map(ReplEntry::Command);
            }
        }
        lines.push(line);
        if joined_size(&lines) > MAX_REPL_GOAL_BYTES {
            return Err(InputFailure::TooLarge);
        }
        return Ok(ReplEntry::Goal(lines.join("\n")));
    }
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<String>, InputFailure> {
    let mut bytes = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Ok(Some("\u{3}".to_owned()));
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(InputFailure::TooLarge);
            }
            Err(_) => return Err(InputFailure::Unavailable),
        };
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(take) > MAX_REPL_LINE_BYTES + 2 {
            let line_ended = available.get(take.saturating_sub(1)) == Some(&b'\n');
            reader.consume(take);
            if !line_ended {
                drain_line(reader)?;
            }
            return Err(InputFailure::TooLarge);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| InputFailure::Unavailable)
}

fn drain_line(reader: &mut impl BufRead) -> Result<(), InputFailure> {
    loop {
        let available = reader.fill_buf().map_err(|_| InputFailure::Unavailable)?;
        if available.is_empty() {
            return Ok(());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let done = available.get(take.saturating_sub(1)) == Some(&b'\n');
        reader.consume(take);
        if done {
            return Ok(());
        }
    }
}

fn has_continuation(line: &str) -> bool {
    line.as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

fn joined_size(lines: &[String]) -> usize {
    lines
        .iter()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(lines.len().saturating_sub(1))
}

fn parse_command(value: &str) -> Result<ReplCommand, InputFailure> {
    let parts = value.split_ascii_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["/model"] => Ok(ReplCommand::Model(None)),
        ["/model", name] => Ok(ReplCommand::Model(Some((*name).to_owned()))),
        ["/status"] => Ok(ReplCommand::Status),
        ["/permissions"] => Ok(ReplCommand::Permissions(None)),
        ["/permissions", kind, mode] => Ok(ReplCommand::Permissions(Some((
            PermissionKind::parse(kind).ok_or(InputFailure::InvalidCommand)?,
            PermissionMode::parse(mode).ok_or(InputFailure::InvalidCommand)?,
        )))),
        ["/resume"] => Ok(ReplCommand::Resume(None)),
        ["/resume", run_id] => Ok(ReplCommand::Resume(Some((*run_id).to_owned()))),
        ["/clear"] => Ok(ReplCommand::Clear),
        ["/help"] => Ok(ReplCommand::Help),
        ["/exit" | "/quit"] => Ok(ReplCommand::Exit),
        _ => Err(InputFailure::InvalidCommand),
    }
}

fn print_help(output: &mut impl Write) -> io::Result<()> {
    writeln!(
        output,
        "/model [PROFILE]                 show or select a model profile"
    )?;
    writeln!(
        output,
        "/status                          show the current durable Run"
    )?;
    writeln!(
        output,
        "/permissions                     show approval modes and tool catalog"
    )?;
    writeln!(
        output,
        "/permissions KIND ask|allow|deny change model/read/write/execute mode"
    )?;
    writeln!(
        output,
        "/resume [RUN_ID]                 continue or replay a durable Run"
    )?;
    writeln!(
        output,
        "/clear                           detach without deleting durable state"
    )?;
    writeln!(output, "/exit                            leave XGENy")
}

fn print_model(output: &mut impl Write, model: &ModelView) -> io::Result<()> {
    write!(output, "model: profile=")?;
    write_terminal_text(output, &model.profile)?;
    write!(output, " model=")?;
    write_terminal_text(output, &model.model)?;
    writeln!(output, " authentication={}", model.authentication)
}

fn print_permissions(output: &mut impl Write, settings: PermissionSettings) -> io::Result<()> {
    writeln!(
        output,
        "permissions: model={} read={} write={} execute={}",
        settings.model.label(),
        settings.read.label(),
        settings.write.label(),
        settings.execute.label()
    )
}

fn print_failure(output: &mut impl Write, failure: ReplFailure) -> io::Result<()> {
    writeln!(output, "error: {}", failure.code())
}

fn write_terminal_text(output: &mut impl Write, value: &str) -> io::Result<()> {
    for character in value.chars() {
        if character == '\n' || character == '\t' || !character.is_control() {
            write!(output, "{character}")?;
        } else {
            for escaped in character.escape_default() {
                write!(output, "{escaped}")?;
            }
        }
    }
    Ok(())
}

pub(crate) fn discover_developer_executables() -> Vec<(String, PathBuf)> {
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    #[cfg(windows)]
    let extensions = windows_executable_extensions();
    #[cfg(not(windows))]
    let extensions = vec![String::new()];
    discover_developer_executables_in(&path, &extensions, DEVELOPER_EXECUTABLES)
}

fn discover_developer_executables_in(
    search_path: &std::ffi::OsStr,
    extensions: &[String],
    selected_ids: &[&str],
) -> Vec<(String, PathBuf)> {
    let directories = env::split_paths(search_path)
        .filter(|directory| directory.is_absolute())
        .collect::<Vec<_>>();
    let mut found = Vec::new();
    for id in selected_ids {
        let candidate = directories.iter().find_map(|directory| {
            extensions.iter().find_map(|extension| {
                let path = directory.join(format!("{id}{extension}"));
                is_executable_file(&path).then_some(path)
            })
        });
        if let Some(path) = candidate {
            found.push(((*id).to_owned(), path));
        }
    }
    found
}

#[cfg(windows)]
fn windows_executable_extensions() -> Vec<String> {
    let configured = env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.COM".to_owned());
    let mut extensions = configured
        .split(';')
        .filter(|extension| matches!(extension.to_ascii_lowercase().as_str(), ".exe" | ".com"))
        .map(|extension| extension.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if extensions.is_empty() {
        extensions.extend([".exe".to_owned(), ".com".to_owned()]);
    }
    extensions
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    struct FakeHost {
        results: VecDeque<Result<LocalCommandResult, ReplFailure>>,
        starts: Vec<(String, InvocationGrants)>,
        resumes: Vec<(String, InvocationGrants)>,
        tools: Vec<String>,
        model: ModelView,
    }

    impl FakeHost {
        fn new(results: impl IntoIterator<Item = LocalCommandResult>) -> Self {
            Self {
                results: results.into_iter().map(Ok).collect(),
                starts: Vec::new(),
                resumes: Vec::new(),
                tools: vec!["cargo".to_owned()],
                model: ModelView {
                    profile: "default".to_owned(),
                    model: "qwen".to_owned(),
                    authentication: "external_or_none",
                },
            }
        }

        fn next(&mut self) -> Result<LocalCommandResult, ReplFailure> {
            self.results
                .pop_front()
                .unwrap_or_else(|| Err(ReplFailure::new("fixture_exhausted")))
        }
    }

    impl ReplHost for FakeHost {
        fn model(&mut self) -> Result<ModelView, ReplFailure> {
            Ok(self.model.clone())
        }

        fn use_model(&mut self, name: &str) -> Result<ModelView, ReplFailure> {
            self.model.profile = name.to_owned();
            Ok(self.model.clone())
        }

        fn executable_ids(&self) -> &[String] {
            &self.tools
        }

        fn start(
            &mut self,
            goal: String,
            grants: InvocationGrants,
            progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
        ) -> Result<LocalCommandResult, ReplFailure> {
            self.starts.push((goal, grants));
            if progress(DriverProgress::ModelCallStarting) == DriverProgressControl::Cancel {
                return Ok(LocalCommandResult::Paused {
                    run_id: Some("run-44444444444444444444444444444444".to_owned()),
                    reason: PauseReason::UserCancelled,
                });
            }
            self.next()
        }

        fn resume(
            &mut self,
            run_id: &str,
            grants: InvocationGrants,
            progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
        ) -> Result<LocalCommandResult, ReplFailure> {
            self.resumes.push((run_id.to_owned(), grants));
            let _ = progress(DriverProgress::EffectStarting {
                effect_class: xgeny_domain::EffectClass::ReadOnly,
            });
            self.next()
        }
    }

    #[test]
    fn multiline_goal_prompts_each_boundary_and_completes() {
        let mut host = FakeHost::new([
            LocalCommandResult::Paused {
                run_id: Some("run-11111111111111111111111111111111".to_owned()),
                reason: PauseReason::ReadApprovalRequired,
            },
            LocalCommandResult::Completed {
                run_id: "run-11111111111111111111111111111111".to_owned(),
                summary: "done".to_owned(),
            },
        ]);
        let mut input = io::Cursor::new(b"first line\\\nsecond line\ny\ny\n/status\n/exit\n");
        let mut output = Vec::new();
        run(&mut input, &mut output, &mut host, &Cancellation::default()).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(host.starts.len(), 1);
        assert_eq!(host.starts[0].0, "first line\nsecond line");
        assert!(host.starts[0].1.model);
        assert_eq!(host.resumes.len(), 1);
        assert!(host.resumes[0].1.read);
        assert!(!host.resumes[0].1.write);
        assert!(output.contains("progress: model_call_starting"));
        assert!(output.contains("completed: run-11111111111111111111111111111111"));
        assert!(output.contains("status: completed"));
    }

    #[test]
    fn permission_modes_and_clear_do_not_delete_durable_runs() {
        let mut host = FakeHost::new([LocalCommandResult::Paused {
            run_id: Some("run-22222222222222222222222222222222".to_owned()),
            reason: PauseReason::WriteApprovalRequired,
        }]);
        let mut input = io::Cursor::new(
            b"/permissions model allow\n/permissions write deny\nchange file\n/clear\n/status\n/exit\n",
        );
        let mut output = Vec::new();
        run(&mut input, &mut output, &mut host, &Cancellation::default()).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(host.starts.len(), 1);
        assert!(host.starts[0].1.model);
        assert!(output.contains("paused: write_approval_required"));
        assert!(output.contains("session cleared; durable Runs were not deleted"));
        assert!(output.contains("active_run: none"));
    }

    #[test]
    fn completion_output_escapes_terminal_control_characters() {
        let mut host = FakeHost::new([LocalCommandResult::Completed {
            run_id: "run-33333333333333333333333333333333".to_owned(),
            summary: "safe\u{1b}[31m".to_owned(),
        }]);
        let mut input = io::Cursor::new(b"goal\ny\n/exit\n");
        let mut output = Vec::new();
        run(&mut input, &mut output, &mut host, &Cancellation::default()).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains('\u{1b}'));
        assert!(output.contains("safe\\u{1b}[31m"));
    }

    #[test]
    fn cancellation_stops_at_observed_boundary_and_keeps_run_resumable() {
        let mut host = FakeHost::new([]);
        let cancellation = Cancellation::default();
        cancellation.request();
        let mut input = io::Cursor::new(b"/permissions model allow\ngoal\n/exit\n");
        let mut output = Vec::new();

        run(&mut input, &mut output, &mut host, &cancellation).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("paused: user_cancelled"));
        assert!(output.contains("bye"));
        assert_eq!(host.starts.len(), 1);
    }

    #[test]
    fn next_goal_receives_bounded_previous_durable_result_as_untrusted_context() {
        let mut host = FakeHost::new([
            LocalCommandResult::Completed {
                run_id: "run-55555555555555555555555555555555".to_owned(),
                summary: "first durable result".to_owned(),
            },
            LocalCommandResult::Completed {
                run_id: "run-66666666666666666666666666666666".to_owned(),
                summary: "second durable result".to_owned(),
            },
        ]);
        let mut input =
            io::Cursor::new(b"/permissions model allow\nfirst goal\nsecond goal\n/exit\n");
        let mut output = Vec::new();

        run(&mut input, &mut output, &mut host, &Cancellation::default()).unwrap();

        assert_eq!(host.starts[0].0, "first goal");
        assert!(host.starts[1].0.contains("Current user goal:\nsecond goal"));
        assert!(host.starts[1].0.contains("first durable result"));
        assert!(
            host.starts[1]
                .0
                .contains("untrusted context; revalidate before acting")
        );
    }

    #[test]
    #[cfg(unix)]
    fn executable_discovery_ignores_relative_path_entries() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = tempdir().unwrap();
        let absolute = fixture.path().join("bin");
        fs::create_dir(&absolute).unwrap();
        let cargo = absolute.join("cargo");
        fs::write(&cargo, b"fixture").unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
        let search = env::join_paths([PathBuf::from("."), absolute]).unwrap();

        let found = discover_developer_executables_in(&search, &[String::new()], &["cargo"]);
        assert_eq!(found, vec![("cargo".to_owned(), cargo)]);
    }

    #[test]
    fn oversized_line_is_drained_before_the_next_command() {
        let mut bytes = vec![b'x'; MAX_REPL_LINE_BYTES + 3];
        bytes.extend_from_slice(b"\n/status\n");
        let mut input = io::Cursor::new(bytes);
        assert_eq!(read_bounded_line(&mut input), Err(InputFailure::TooLarge));
        assert_eq!(
            read_bounded_line(&mut input).unwrap(),
            Some("/status".to_owned())
        );
    }
}
