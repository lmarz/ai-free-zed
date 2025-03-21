use crate::{
    buffer_store::{BufferStore, BufferStoreEvent},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
    Project, ProjectEnvironment, ProjectItem, ProjectPath,
};
use anyhow::{Context as _, Result};
use askpass::{AskPassDelegate, AskPassSession};
use buffer_diff::BufferDiffEvent;
use client::ProjectId;
use collections::HashMap;
use fs::Fs;
use futures::{
    channel::{mpsc, oneshot},
    future::OptionFuture,
    StreamExt as _,
};
use git::repository::DiffType;
use git::{
    repository::{
        Branch, CommitDetails, GitRepository, PushOptions, Remote, RemoteCommandOutput, RepoPath,
        ResetMode,
    },
    status::FileStatus,
};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    WeakEntity,
};
use language::{Buffer, LanguageRegistry};
use parking_lot::Mutex;
use rpc::{
    proto::{self, git_reset, ToProto},
    AnyProtoClient, TypedEnvelope,
};
use settings::WorktreeId;
use std::{
    collections::VecDeque,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};

use text::BufferId;
use util::{debug_panic, maybe, ResultExt};
use worktree::{ProjectEntryId, RepositoryEntry, StatusEntry, WorkDirectory};

pub struct GitStore {
    state: GitStoreState,
    buffer_store: Entity<BufferStore>,
    repositories: Vec<Entity<Repository>>,
    active_index: Option<usize>,
    update_sender: mpsc::UnboundedSender<GitJob>,
    _subscriptions: [Subscription; 2],
}

enum GitStoreState {
    Local {
        client: AnyProtoClient,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
    },
    Ssh {
        environment: Entity<ProjectEnvironment>,
        upstream_client: AnyProtoClient,
        project_id: ProjectId,
    },
    Remote {
        upstream_client: AnyProtoClient,
        project_id: ProjectId,
    },
}

pub struct Repository {
    commit_message_buffer: Option<Entity<Buffer>>,
    git_store: WeakEntity<GitStore>,
    project_environment: Option<WeakEntity<ProjectEnvironment>>,
    pub worktree_id: WorktreeId,
    pub repository_entry: RepositoryEntry,
    pub dot_git_abs_path: PathBuf,
    pub worktree_abs_path: Arc<Path>,
    pub is_from_single_file_worktree: bool,
    pub git_repo: GitRepo,
    pub merge_message: Option<String>,
    job_sender: mpsc::UnboundedSender<GitJob>,
    askpass_delegates: Arc<Mutex<HashMap<u64, AskPassDelegate>>>,
    latest_askpass_id: u64,
}

#[derive(Clone)]
pub enum GitRepo {
    Local(Arc<dyn GitRepository>),
    Remote {
        project_id: ProjectId,
        client: AnyProtoClient,
        worktree_id: WorktreeId,
        work_directory_id: ProjectEntryId,
    },
}

#[derive(Debug)]
pub enum GitEvent {
    ActiveRepositoryChanged,
    FileSystemUpdated,
    GitStateUpdated,
    IndexWriteError(anyhow::Error),
}

struct GitJob {
    job: Box<dyn FnOnce(&mut AsyncApp) -> Task<()>>,
    key: Option<GitJobKey>,
}

#[derive(PartialEq, Eq)]
enum GitJobKey {
    WriteIndex(RepoPath),
}

impl EventEmitter<GitEvent> for GitStore {}

impl GitStore {
    pub fn local(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        client: AnyProtoClient,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let update_sender = Self::spawn_git_worker(cx);
        let _subscriptions = [
            cx.subscribe(worktree_store, Self::on_worktree_store_event),
            cx.subscribe(&buffer_store, Self::on_buffer_store_event),
        ];

        let state = GitStoreState::Local {
            client,
            environment,
            fs,
        };

        GitStore {
            state,
            buffer_store,
            repositories: Vec::new(),
            active_index: None,
            update_sender,
            _subscriptions,
        }
    }

    pub fn remote(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        upstream_client: AnyProtoClient,
        project_id: ProjectId,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let update_sender = Self::spawn_git_worker(cx);
        let _subscriptions = [
            cx.subscribe(worktree_store, Self::on_worktree_store_event),
            cx.subscribe(&buffer_store, Self::on_buffer_store_event),
        ];

        let state = GitStoreState::Remote {
            upstream_client,
            project_id,
        };

        GitStore {
            state,
            buffer_store,
            repositories: Vec::new(),
            active_index: None,
            update_sender,
            _subscriptions,
        }
    }

    pub fn ssh(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        environment: Entity<ProjectEnvironment>,
        upstream_client: AnyProtoClient,
        project_id: ProjectId,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let update_sender = Self::spawn_git_worker(cx);
        let _subscriptions = [
            cx.subscribe(worktree_store, Self::on_worktree_store_event),
            cx.subscribe(&buffer_store, Self::on_buffer_store_event),
        ];

        let state = GitStoreState::Ssh {
            upstream_client,
            project_id,
            environment,
        };

        GitStore {
            state,
            buffer_store,
            repositories: Vec::new(),
            active_index: None,
            update_sender,
            _subscriptions,
        }
    }

    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_get_remotes);
        client.add_entity_request_handler(Self::handle_get_branches);
        client.add_entity_request_handler(Self::handle_change_branch);
        client.add_entity_request_handler(Self::handle_create_branch);
        client.add_entity_request_handler(Self::handle_git_init);
        client.add_entity_request_handler(Self::handle_push);
        client.add_entity_request_handler(Self::handle_pull);
        client.add_entity_request_handler(Self::handle_fetch);
        client.add_entity_request_handler(Self::handle_stage);
        client.add_entity_request_handler(Self::handle_unstage);
        client.add_entity_request_handler(Self::handle_commit);
        client.add_entity_request_handler(Self::handle_reset);
        client.add_entity_request_handler(Self::handle_show);
        client.add_entity_request_handler(Self::handle_checkout_files);
        client.add_entity_request_handler(Self::handle_open_commit_message_buffer);
        client.add_entity_request_handler(Self::handle_set_index_text);
        client.add_entity_request_handler(Self::handle_askpass);
        client.add_entity_request_handler(Self::handle_check_for_pushed_commits);
        client.add_entity_request_handler(Self::handle_git_diff);
    }

    pub fn active_repository(&self) -> Option<Entity<Repository>> {
        self.active_index
            .map(|index| self.repositories[index].clone())
    }

    fn client(&self) -> AnyProtoClient {
        match &self.state {
            GitStoreState::Local { client, .. } => client.clone(),
            GitStoreState::Ssh {
                upstream_client, ..
            } => upstream_client.clone(),
            GitStoreState::Remote {
                upstream_client, ..
            } => upstream_client.clone(),
        }
    }

    fn project_environment(&self) -> Option<Entity<ProjectEnvironment>> {
        match &self.state {
            GitStoreState::Local { environment, .. } => Some(environment.clone()),
            GitStoreState::Ssh { environment, .. } => Some(environment.clone()),
            GitStoreState::Remote { .. } => None,
        }
    }

    fn project_id(&self) -> Option<ProjectId> {
        match &self.state {
            GitStoreState::Local { .. } => None,
            GitStoreState::Ssh { project_id, .. } => Some(*project_id),
            GitStoreState::Remote { project_id, .. } => Some(*project_id),
        }
    }

    fn on_worktree_store_event(
        &mut self,
        worktree_store: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let mut new_repositories = Vec::new();
        let mut new_active_index = None;
        let this = cx.weak_entity();
        let client = self.client();
        let project_id = self.project_id();

        worktree_store.update(cx, |worktree_store, cx| {
            for worktree in worktree_store.worktrees() {
                worktree.update(cx, |worktree, cx| {
                    let snapshot = worktree.snapshot();
                    for repo in snapshot.repositories().iter() {
                        let git_data = worktree
                            .as_local()
                            .and_then(|local_worktree| local_worktree.get_local_repo(repo))
                            .map(|local_repo| {
                                (
                                    GitRepo::Local(local_repo.repo().clone()),
                                    local_repo.merge_message.clone(),
                                )
                            })
                            .or_else(|| {
                                let client = client.clone();
                                let project_id = project_id?;
                                Some((
                                    GitRepo::Remote {
                                        project_id,
                                        client,
                                        worktree_id: worktree.id(),
                                        work_directory_id: repo.work_directory_id(),
                                    },
                                    None,
                                ))
                            });
                        let Some((git_repo, merge_message)) = git_data else {
                            continue;
                        };
                        let worktree_id = worktree.id();
                        let existing =
                            self.repositories
                                .iter()
                                .enumerate()
                                .find(|(_, existing_handle)| {
                                    existing_handle.read(cx).id()
                                        == (worktree_id, repo.work_directory_id())
                                });
                        let handle = if let Some((index, handle)) = existing {
                            if self.active_index == Some(index) {
                                new_active_index = Some(new_repositories.len());
                            }
                            // Update the statuses and merge message but keep everything else.
                            let existing_handle = handle.clone();
                            existing_handle.update(cx, |existing_handle, _| {
                                existing_handle.repository_entry = repo.clone();
                                if matches!(git_repo, GitRepo::Local { .. }) {
                                    existing_handle.merge_message = merge_message;
                                }
                            });
                            existing_handle
                        } else {
                            let environment = self.project_environment();
                            cx.new(|_| Repository {
                                project_environment: environment
                                    .as_ref()
                                    .map(|env| env.downgrade()),
                                git_store: this.clone(),
                                worktree_id,
                                askpass_delegates: Default::default(),
                                latest_askpass_id: 0,
                                repository_entry: repo.clone(),
                                dot_git_abs_path: worktree.dot_git_abs_path(&repo.work_directory),
                                worktree_abs_path: worktree.abs_path(),
                                is_from_single_file_worktree: worktree.is_single_file(),
                                git_repo,
                                job_sender: self.update_sender.clone(),
                                merge_message,
                                commit_message_buffer: None,
                            })
                        };
                        new_repositories.push(handle);
                    }
                })
            }
        });

        if new_active_index == None && new_repositories.len() > 0 {
            new_active_index = Some(0);
        }

        self.repositories = new_repositories;
        self.active_index = new_active_index;

        match event {
            WorktreeStoreEvent::WorktreeUpdatedGitRepositories(_) => {
                cx.emit(GitEvent::GitStateUpdated);
            }
            _ => {
                cx.emit(GitEvent::FileSystemUpdated);
            }
        }
    }

    fn on_buffer_store_event(
        &mut self,
        _: Entity<BufferStore>,
        event: &BufferStoreEvent,
        cx: &mut Context<'_, Self>,
    ) {
        if let BufferStoreEvent::BufferDiffAdded(diff) = event {
            cx.subscribe(diff, Self::on_buffer_diff_event).detach();
        }
    }

    fn on_buffer_diff_event(
        this: &mut GitStore,
        diff: Entity<buffer_diff::BufferDiff>,
        event: &BufferDiffEvent,
        cx: &mut Context<'_, GitStore>,
    ) {
        if let BufferDiffEvent::HunksStagedOrUnstaged(new_index_text) = event {
            let buffer_id = diff.read(cx).buffer_id;
            if let Some((repo, path)) = this.repository_and_path_for_buffer_id(buffer_id, cx) {
                let recv = repo.update(cx, |repo, cx| {
                    repo.set_index_text(
                        path,
                        new_index_text.as_ref().map(|rope| rope.to_string()),
                        cx,
                    )
                });
                let diff = diff.downgrade();
                cx.spawn(|this, mut cx| async move {
                    if let Some(result) = cx.background_spawn(async move { recv.await.ok() }).await
                    {
                        if let Err(error) = result {
                            diff.update(&mut cx, |diff, cx| {
                                diff.clear_pending_hunks(cx);
                            })
                            .ok();
                            this.update(&mut cx, |_, cx| cx.emit(GitEvent::IndexWriteError(error)))
                                .ok();
                        }
                    }
                })
                .detach();
            }
        }
    }

    pub fn all_repositories(&self) -> Vec<Entity<Repository>> {
        self.repositories.clone()
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        let (repo, path) = self.repository_and_path_for_buffer_id(buffer_id, cx)?;
        let status = repo.read(cx).repository_entry.status_for_path(&path)?;
        Some(status.status)
    }

    fn repository_and_path_for_buffer_id(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(Entity<Repository>, RepoPath)> {
        let buffer = self.buffer_store.read(cx).get(buffer_id)?;
        let path = buffer.read(cx).project_path(cx)?;
        let mut result: Option<(Entity<Repository>, RepoPath)> = None;
        for repo_handle in &self.repositories {
            let repo = repo_handle.read(cx);
            if repo.worktree_id == path.worktree_id {
                if let Ok(relative_path) = repo.repository_entry.relativize(&path.path) {
                    if result
                        .as_ref()
                        .is_none_or(|(result, _)| !repo.contains_sub_repo(result, cx))
                    {
                        result = Some((repo_handle.clone(), relative_path))
                    }
                }
            }
        }
        result
    }

    fn spawn_git_worker(cx: &mut Context<'_, GitStore>) -> mpsc::UnboundedSender<GitJob> {
        let (job_tx, mut job_rx) = mpsc::unbounded::<GitJob>();

        cx.spawn(|_, mut cx| async move {
            let mut jobs = VecDeque::new();
            loop {
                while let Ok(Some(next_job)) = job_rx.try_next() {
                    jobs.push_back(next_job);
                }

                if let Some(job) = jobs.pop_front() {
                    if let Some(current_key) = &job.key {
                        if jobs
                            .iter()
                            .any(|other_job| other_job.key.as_ref() == Some(current_key))
                        {
                            continue;
                        }
                    }
                    (job.job)(&mut cx).await;
                } else if let Some(job) = job_rx.next().await {
                    jobs.push_back(job);
                } else {
                    break;
                }
            }
        })
        .detach();
        job_tx
    }

    pub fn git_init(
        &self,
        path: Arc<Path>,
        fallback_branch_name: String,
        cx: &App,
    ) -> Task<Result<()>> {
        match &self.state {
            GitStoreState::Local { fs, .. } => {
                let fs = fs.clone();
                cx.background_executor()
                    .spawn(async move { fs.git_init(&path, fallback_branch_name) })
            }
            GitStoreState::Ssh {
                upstream_client,
                project_id,
                ..
            }
            | GitStoreState::Remote {
                upstream_client,
                project_id,
            } => {
                let client = upstream_client.clone();
                let project_id = *project_id;
                cx.background_executor().spawn(async move {
                    client
                        .request(proto::GitInit {
                            project_id: project_id.0,
                            abs_path: path.to_string_lossy().to_string(),
                            fallback_branch_name,
                        })
                        .await?;
                    Ok(())
                })
            }
        }
    }

    async fn handle_git_init(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitInit>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let path: Arc<Path> = PathBuf::from(envelope.payload.abs_path).into();
        let name = envelope.payload.fallback_branch_name;
        cx.update(|cx| this.read(cx).git_init(path, name, cx))?
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_fetch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Fetch>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let askpass_id = envelope.payload.askpass_id;

        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            worktree_id,
            work_directory_id,
            askpass_id,
            &mut cx,
        );

        let remote_output = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.fetch(askpass, cx)
            })?
            .await??;

        Ok(proto::RemoteMessageResponse {
            stdout: remote_output.stdout,
            stderr: remote_output.stderr,
        })
    }

    async fn handle_push(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Push>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let askpass_id = envelope.payload.askpass_id;
        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            worktree_id,
            work_directory_id,
            askpass_id,
            &mut cx,
        );

        let options = envelope
            .payload
            .options
            .as_ref()
            .map(|_| match envelope.payload.options() {
                proto::push::PushOptions::SetUpstream => git::repository::PushOptions::SetUpstream,
                proto::push::PushOptions::Force => git::repository::PushOptions::Force,
            });

        let branch_name = envelope.payload.branch_name.into();
        let remote_name = envelope.payload.remote_name.into();

        let remote_output = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.push(branch_name, remote_name, options, askpass, cx)
            })?
            .await??;
        Ok(proto::RemoteMessageResponse {
            stdout: remote_output.stdout,
            stderr: remote_output.stderr,
        })
    }

    async fn handle_pull(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Pull>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let askpass_id = envelope.payload.askpass_id;
        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            worktree_id,
            work_directory_id,
            askpass_id,
            &mut cx,
        );

        let branch_name = envelope.payload.branch_name.into();
        let remote_name = envelope.payload.remote_name.into();

        let remote_message = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.pull(branch_name, remote_name, askpass, cx)
            })?
            .await??;

        Ok(proto::RemoteMessageResponse {
            stdout: remote_message.stdout,
            stderr: remote_message.stderr,
        })
    }

    async fn handle_stage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Stage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(PathBuf::from)
            .map(RepoPath::new)
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stage_entries(entries, cx)
            })?
            .await?;
        Ok(proto::Ack {})
    }

    async fn handle_unstage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Unstage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(PathBuf::from)
            .map(RepoPath::new)
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.unstage_entries(entries, cx)
            })?
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_set_index_text(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetIndexText>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.set_index_text(
                    RepoPath::from_str(&envelope.payload.path),
                    envelope.payload.text,
                    cx,
                )
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_commit(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Commit>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let message = SharedString::from(envelope.payload.message);
        let name = envelope.payload.name.map(SharedString::from);
        let email = envelope.payload.email.map(SharedString::from);

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.commit(message, name.zip(email), cx)
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_get_remotes(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetRemotes>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetRemotesResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let branch_name = envelope.payload.branch_name;

        let remotes = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.get_remotes(branch_name)
            })?
            .await??;

        Ok(proto::GetRemotesResponse {
            remotes: remotes
                .into_iter()
                .map(|remotes| proto::get_remotes_response::Remote {
                    name: remotes.name.to_string(),
                })
                .collect::<Vec<_>>(),
        })
    }

    async fn handle_get_branches(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitGetBranches>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitBranchesResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let branches = repository_handle
            .update(&mut cx, |repository_handle, _| repository_handle.branches())?
            .await??;

        Ok(proto::GitBranchesResponse {
            branches: branches
                .into_iter()
                .map(|branch| worktree::branch_to_proto(&branch))
                .collect::<Vec<_>>(),
        })
    }
    async fn handle_create_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.create_branch(branch_name)
            })?
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_change_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitChangeBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.change_branch(branch_name)
            })?
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_show(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitShow>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCommitDetails> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let commit = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.show(envelope.payload.commit)
            })?
            .await??;
        Ok(proto::GitCommitDetails {
            sha: commit.sha.into(),
            message: commit.message.into(),
            commit_timestamp: commit.commit_timestamp,
            committer_email: commit.committer_email.into(),
            committer_name: commit.committer_name.into(),
        })
    }

    async fn handle_reset(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitReset>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let mode = match envelope.payload.mode() {
            git_reset::ResetMode::Soft => ResetMode::Soft,
            git_reset::ResetMode::Mixed => ResetMode::Mixed,
        };

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.reset(envelope.payload.commit, mode, cx)
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_checkout_files(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCheckoutFiles>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let paths = envelope
            .payload
            .paths
            .iter()
            .map(|s| RepoPath::from_str(s))
            .collect();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.checkout_files(&envelope.payload.commit, paths, cx)
            })?
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_open_commit_message_buffer(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::OpenCommitMessageBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let buffer = repository
            .update(&mut cx, |repository, cx| {
                repository.open_commit_buffer(None, this.read(cx).buffer_store.clone(), cx)
            })?
            .await?;

        let buffer_id = buffer.read_with(&cx, |buffer, _| buffer.remote_id())?;
        this.update(&mut cx, |this, cx| {
            this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store
                    .create_buffer_for_peer(
                        &buffer,
                        envelope.original_sender_id.unwrap_or(envelope.sender_id),
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
        })?;

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_askpass(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AskPassRequest>,
        mut cx: AsyncApp,
    ) -> Result<proto::AskPassResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let delegates = cx.update(|cx| repository.read(cx).askpass_delegates.clone())?;
        let Some(mut askpass) = delegates.lock().remove(&envelope.payload.askpass_id) else {
            debug_panic!("no askpass found");
            return Err(anyhow::anyhow!("no askpass found"));
        };

        let response = askpass.ask_password(envelope.payload.prompt).await?;

        delegates
            .lock()
            .insert(envelope.payload.askpass_id, askpass);

        Ok(proto::AskPassResponse { response })
    }

    async fn handle_check_for_pushed_commits(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CheckForPushedCommits>,
        mut cx: AsyncApp,
    ) -> Result<proto::CheckForPushedCommitsResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;

        let branches = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.check_for_pushed_commits()
            })?
            .await??;
        Ok(proto::CheckForPushedCommitsResponse {
            pushed_to: branches
                .into_iter()
                .map(|commit| commit.to_string())
                .collect(),
        })
    }

    async fn handle_git_diff(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitDiffResponse> {
        let worktree_id = WorktreeId::from_proto(envelope.payload.worktree_id);
        let work_directory_id = ProjectEntryId::from_proto(envelope.payload.work_directory_id);
        let repository_handle =
            Self::repository_for_request(&this, worktree_id, work_directory_id, &mut cx)?;
        let diff_type = match envelope.payload.diff_type() {
            proto::git_diff::DiffType::HeadToIndex => DiffType::HeadToIndex,
            proto::git_diff::DiffType::HeadToWorktree => DiffType::HeadToWorktree,
        };

        let mut diff = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.diff(diff_type, cx)
            })?
            .await??;
        const ONE_MB: usize = 1_000_000;
        if diff.len() > ONE_MB {
            diff = diff.chars().take(ONE_MB).collect()
        }

        Ok(proto::GitDiffResponse { diff })
    }

    fn repository_for_request(
        this: &Entity<Self>,
        worktree_id: WorktreeId,
        work_directory_id: ProjectEntryId,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Repository>> {
        this.update(cx, |this, cx| {
            this.repositories
                .iter()
                .find(|repository_handle| {
                    repository_handle.read(cx).worktree_id == worktree_id
                        && repository_handle
                            .read(cx)
                            .repository_entry
                            .work_directory_id()
                            == work_directory_id
                })
                .context("missing repository handle")
                .cloned()
        })?
    }
}

fn make_remote_delegate(
    this: Entity<GitStore>,
    project_id: u64,
    worktree_id: WorktreeId,
    work_directory_id: ProjectEntryId,
    askpass_id: u64,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    AskPassDelegate::new(cx, move |prompt, tx, cx| {
        this.update(cx, |this, cx| {
            let response = this.client().request(proto::AskPassRequest {
                project_id,
                worktree_id: worktree_id.to_proto(),
                work_directory_id: work_directory_id.to_proto(),
                askpass_id,
                prompt,
            });
            cx.spawn(|_, _| async move {
                tx.send(response.await?.response).ok();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        })
        .log_err();
    })
}

impl GitRepo {}

impl Repository {
    pub fn git_store(&self) -> Option<Entity<GitStore>> {
        self.git_store.upgrade()
    }

    fn id(&self) -> (WorktreeId, ProjectEntryId) {
        (self.worktree_id, self.repository_entry.work_directory_id())
    }

    pub fn current_branch(&self) -> Option<&Branch> {
        self.repository_entry.branch()
    }

    fn send_job<F, Fut, R>(&self, job: F) -> oneshot::Receiver<R>
    where
        F: FnOnce(GitRepo, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.send_keyed_job(None, job)
    }

    fn send_keyed_job<F, Fut, R>(&self, key: Option<GitJobKey>, job: F) -> oneshot::Receiver<R>
    where
        F: FnOnce(GitRepo, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = futures::channel::oneshot::channel();
        let git_repo = self.git_repo.clone();
        self.job_sender
            .unbounded_send(GitJob {
                key,
                job: Box::new(|cx: &mut AsyncApp| {
                    let job = job(git_repo, cx.clone());
                    cx.spawn(|_| async move {
                        let result = job.await;
                        result_tx.send(result).ok();
                    })
                }),
            })
            .ok();
        result_rx
    }

    pub fn display_name(&self, project: &Project, cx: &App) -> SharedString {
        maybe!({
            let project_path = self.repo_path_to_project_path(&"".into())?;
            let worktree_name = project
                .worktree_for_id(project_path.worktree_id, cx)?
                .read(cx)
                .root_name();

            let mut path = PathBuf::new();
            path = path.join(worktree_name);
            path = path.join(project_path.path);
            Some(path.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| self.repository_entry.work_directory.display_name())
        .into()
    }

    pub fn activate(&self, cx: &mut Context<Self>) {
        let Some(git_store) = self.git_store.upgrade() else {
            return;
        };
        let entity = cx.entity();
        git_store.update(cx, |git_store, cx| {
            let Some(index) = git_store
                .repositories
                .iter()
                .position(|handle| *handle == entity)
            else {
                return;
            };
            git_store.active_index = Some(index);
            cx.emit(GitEvent::ActiveRepositoryChanged);
        });
    }

    pub fn status(&self) -> impl '_ + Iterator<Item = StatusEntry> {
        self.repository_entry.status()
    }

    pub fn has_conflict(&self, path: &RepoPath) -> bool {
        self.repository_entry
            .current_merge_conflicts
            .contains(&path)
    }

    pub fn repo_path_to_project_path(&self, path: &RepoPath) -> Option<ProjectPath> {
        let path = self.repository_entry.try_unrelativize(path)?;
        Some((self.worktree_id, path).into())
    }

    pub fn project_path_to_repo_path(&self, path: &ProjectPath) -> Option<RepoPath> {
        self.worktree_id_path_to_repo_path(path.worktree_id, &path.path)
    }

    // note: callers must verify these come from the same worktree
    pub fn contains_sub_repo(&self, other: &Entity<Self>, cx: &App) -> bool {
        let other_work_dir = &other.read(cx).repository_entry.work_directory;
        match (&self.repository_entry.work_directory, other_work_dir) {
            (WorkDirectory::InProject { .. }, WorkDirectory::AboveProject { .. }) => false,
            (WorkDirectory::AboveProject { .. }, WorkDirectory::InProject { .. }) => true,
            (
                WorkDirectory::InProject {
                    relative_path: this_path,
                },
                WorkDirectory::InProject {
                    relative_path: other_path,
                },
            ) => other_path.starts_with(this_path),
            (
                WorkDirectory::AboveProject {
                    absolute_path: this_path,
                    ..
                },
                WorkDirectory::AboveProject {
                    absolute_path: other_path,
                    ..
                },
            ) => other_path.starts_with(this_path),
        }
    }

    pub fn worktree_id_path_to_repo_path(
        &self,
        worktree_id: WorktreeId,
        path: &Path,
    ) -> Option<RepoPath> {
        if worktree_id != self.worktree_id {
            return None;
        }
        self.repository_entry.relativize(path).log_err()
    }

    pub fn open_commit_buffer(
        &mut self,
        languages: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        if let Some(buffer) = self.commit_message_buffer.clone() {
            return Task::ready(Ok(buffer));
        }

        if let GitRepo::Remote {
            project_id,
            client,
            worktree_id,
            work_directory_id,
        } = self.git_repo.clone()
        {
            let client = client.clone();
            cx.spawn(|repository, mut cx| async move {
                let request = client.request(proto::OpenCommitMessageBuffer {
                    project_id: project_id.0,
                    worktree_id: worktree_id.to_proto(),
                    work_directory_id: work_directory_id.to_proto(),
                });
                let response = request.await.context("requesting to open commit buffer")?;
                let buffer_id = BufferId::new(response.buffer_id)?;
                let buffer = buffer_store
                    .update(&mut cx, |buffer_store, cx| {
                        buffer_store.wait_for_remote_buffer(buffer_id, cx)
                    })?
                    .await?;
                if let Some(language_registry) = languages {
                    let git_commit_language =
                        language_registry.language_for_name("Git Commit").await?;
                    buffer.update(&mut cx, |buffer, cx| {
                        buffer.set_language(Some(git_commit_language), cx);
                    })?;
                }
                repository.update(&mut cx, |repository, _| {
                    repository.commit_message_buffer = Some(buffer.clone());
                })?;
                Ok(buffer)
            })
        } else {
            self.open_local_commit_buffer(languages, buffer_store, cx)
        }
    }

    fn open_local_commit_buffer(
        &mut self,
        language_registry: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        cx.spawn(|repository, mut cx| async move {
            let buffer = buffer_store
                .update(&mut cx, |buffer_store, cx| buffer_store.create_buffer(cx))?
                .await?;

            if let Some(language_registry) = language_registry {
                let git_commit_language = language_registry.language_for_name("Git Commit").await?;
                buffer.update(&mut cx, |buffer, cx| {
                    buffer.set_language(Some(git_commit_language), cx);
                })?;
            }

            repository.update(&mut cx, |repository, _| {
                repository.commit_message_buffer = Some(buffer.clone());
            })?;
            Ok(buffer)
        })
    }

    pub fn checkout_files(
        &self,
        commit: &str,
        paths: Vec<RepoPath>,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let commit = commit.to_string();
        let env = self.worktree_environment(cx);

        self.send_job(|git_repo, _| async move {
            match git_repo {
                GitRepo::Local(repo) => repo.checkout_files(commit, paths, env.await).await,
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    client
                        .request(proto::GitCheckoutFiles {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            commit,
                            paths: paths
                                .into_iter()
                                .map(|p| p.to_string_lossy().to_string())
                                .collect(),
                        })
                        .await?;

                    Ok(())
                }
            }
        })
    }

    pub fn reset(
        &self,
        commit: String,
        reset_mode: ResetMode,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let commit = commit.to_string();
        let env = self.worktree_environment(cx);
        self.send_job(|git_repo, _| async move {
            match git_repo {
                GitRepo::Local(git_repo) => {
                    let env = env.await;
                    git_repo.reset(commit, reset_mode, env).await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    client
                        .request(proto::GitReset {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            commit,
                            mode: match reset_mode {
                                ResetMode::Soft => git_reset::ResetMode::Soft.into(),
                                ResetMode::Mixed => git_reset::ResetMode::Mixed.into(),
                            },
                        })
                        .await?;

                    Ok(())
                }
            }
        })
    }

    pub fn show(&self, commit: String) -> oneshot::Receiver<Result<CommitDetails>> {
        self.send_job(|git_repo, cx| async move {
            match git_repo {
                GitRepo::Local(git_repository) => git_repository.show(commit, cx).await,
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    let resp = client
                        .request(proto::GitShow {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            commit,
                        })
                        .await?;

                    Ok(CommitDetails {
                        sha: resp.sha.into(),
                        message: resp.message.into(),
                        commit_timestamp: resp.commit_timestamp,
                        committer_email: resp.committer_email.into(),
                        committer_name: resp.committer_name.into(),
                    })
                }
            }
        })
    }

    fn buffer_store(&self, cx: &App) -> Option<Entity<BufferStore>> {
        Some(self.git_store.upgrade()?.read(cx).buffer_store.clone())
    }

    pub fn stage_entries(
        &self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if entries.is_empty() {
            return Task::ready(Ok(()));
        }
        let env = self.worktree_environment(cx);

        let mut save_futures = Vec::new();
        if let Some(buffer_store) = self.buffer_store(cx) {
            buffer_store.update(cx, |buffer_store, cx| {
                for path in &entries {
                    let Some(path) = self.repository_entry.try_unrelativize(path) else {
                        continue;
                    };
                    let project_path = (self.worktree_id, path).into();
                    if let Some(buffer) = buffer_store.get_by_path(&project_path, cx) {
                        if buffer
                            .read(cx)
                            .file()
                            .map_or(false, |file| file.disk_state().exists())
                        {
                            save_futures.push(buffer_store.save_buffer(buffer, cx));
                        }
                    }
                }
            })
        }

        cx.spawn(|this, mut cx| async move {
            for save_future in save_futures {
                save_future.await?;
            }
            let env = env.await;

            this.update(&mut cx, |this, _| {
                this.send_job(|git_repo, cx| async move {
                    match git_repo {
                        GitRepo::Local(repo) => repo.stage_paths(entries, env, cx).await,
                        GitRepo::Remote {
                            project_id,
                            client,
                            worktree_id,
                            work_directory_id,
                        } => {
                            client
                                .request(proto::Stage {
                                    project_id: project_id.0,
                                    worktree_id: worktree_id.to_proto(),
                                    work_directory_id: work_directory_id.to_proto(),
                                    paths: entries
                                        .into_iter()
                                        .map(|repo_path| repo_path.as_ref().to_proto())
                                        .collect(),
                                })
                                .await
                                .context("sending stage request")?;

                            Ok(())
                        }
                    }
                })
            })?
            .await??;

            Ok(())
        })
    }

    pub fn unstage_entries(
        &self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if entries.is_empty() {
            return Task::ready(Ok(()));
        }
        let env = self.worktree_environment(cx);

        let mut save_futures = Vec::new();
        if let Some(buffer_store) = self.buffer_store(cx) {
            buffer_store.update(cx, |buffer_store, cx| {
                for path in &entries {
                    let Some(path) = self.repository_entry.try_unrelativize(path) else {
                        continue;
                    };
                    let project_path = (self.worktree_id, path).into();
                    if let Some(buffer) = buffer_store.get_by_path(&project_path, cx) {
                        if buffer
                            .read(cx)
                            .file()
                            .map_or(false, |file| file.disk_state().exists())
                        {
                            save_futures.push(buffer_store.save_buffer(buffer, cx));
                        }
                    }
                }
            })
        }

        cx.spawn(move |this, mut cx| async move {
            for save_future in save_futures {
                save_future.await?;
            }
            let env = env.await;

            this.update(&mut cx, |this, _| {
                this.send_job(|git_repo, cx| async move {
                    match git_repo {
                        GitRepo::Local(repo) => repo.unstage_paths(entries, env, cx).await,
                        GitRepo::Remote {
                            project_id,
                            client,
                            worktree_id,
                            work_directory_id,
                        } => {
                            client
                                .request(proto::Unstage {
                                    project_id: project_id.0,
                                    worktree_id: worktree_id.to_proto(),
                                    work_directory_id: work_directory_id.to_proto(),
                                    paths: entries
                                        .into_iter()
                                        .map(|repo_path| repo_path.as_ref().to_proto())
                                        .collect(),
                                })
                                .await
                                .context("sending unstage request")?;

                            Ok(())
                        }
                    }
                })
            })?
            .await??;

            Ok(())
        })
    }

    pub fn stage_all(&self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let to_stage = self
            .repository_entry
            .status()
            .filter(|entry| !entry.status.staging().is_fully_staged())
            .map(|entry| entry.repo_path.clone())
            .collect();
        self.stage_entries(to_stage, cx)
    }

    pub fn unstage_all(&self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let to_unstage = self
            .repository_entry
            .status()
            .filter(|entry| entry.status.staging().has_staged())
            .map(|entry| entry.repo_path.clone())
            .collect();
        self.unstage_entries(to_unstage, cx)
    }

    /// Get a count of all entries in the active repository, including
    /// untracked files.
    pub fn entry_count(&self) -> usize {
        self.repository_entry.status_len()
    }

    fn worktree_environment(
        &self,
        cx: &mut App,
    ) -> impl Future<Output = HashMap<String, String>> + 'static {
        let task = self.project_environment.as_ref().and_then(|env| {
            env.update(cx, |env, cx| {
                env.get_environment(
                    Some(self.worktree_id),
                    Some(self.worktree_abs_path.clone()),
                    cx,
                )
            })
            .ok()
        });
        async move { OptionFuture::from(task).await.flatten().unwrap_or_default() }
    }

    pub fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let env = self.worktree_environment(cx);
        self.send_job(|git_repo, cx| async move {
            match git_repo {
                GitRepo::Local(repo) => {
                    let env = env.await;
                    repo.commit(message, name_and_email, env, cx).await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    let (name, email) = name_and_email.unzip();
                    client
                        .request(proto::Commit {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            message: String::from(message),
                            name: name.map(String::from),
                            email: email.map(String::from),
                        })
                        .await
                        .context("sending commit request")?;

                    Ok(())
                }
            }
        })
    }

    pub fn fetch(
        &mut self,
        askpass: AskPassDelegate,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let executor = cx.background_executor().clone();
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let env = self.worktree_environment(cx);

        self.send_job(move |git_repo, cx| async move {
            match git_repo {
                GitRepo::Local(git_repository) => {
                    let askpass = AskPassSession::new(&executor, askpass).await?;
                    let env = env.await;
                    git_repository.fetch(askpass, env, cx).await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    askpass_delegates.lock().insert(askpass_id, askpass);
                    let _defer = util::defer(|| {
                        let askpass_delegate = askpass_delegates.lock().remove(&askpass_id);
                        debug_assert!(askpass_delegate.is_some());
                    });

                    let response = client
                        .request(proto::Fetch {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            askpass_id,
                        })
                        .await
                        .context("sending fetch request")?;

                    Ok(RemoteCommandOutput {
                        stdout: response.stdout,
                        stderr: response.stderr,
                    })
                }
            }
        })
    }

    pub fn push(
        &mut self,
        branch: SharedString,
        remote: SharedString,
        options: Option<PushOptions>,
        askpass: AskPassDelegate,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let executor = cx.background_executor().clone();
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let env = self.worktree_environment(cx);

        self.send_job(move |git_repo, cx| async move {
            match git_repo {
                GitRepo::Local(git_repository) => {
                    let env = env.await;
                    let askpass = AskPassSession::new(&executor, askpass).await?;
                    git_repository
                        .push(
                            branch.to_string(),
                            remote.to_string(),
                            options,
                            askpass,
                            env,
                            cx,
                        )
                        .await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    askpass_delegates.lock().insert(askpass_id, askpass);
                    let _defer = util::defer(|| {
                        let askpass_delegate = askpass_delegates.lock().remove(&askpass_id);
                        debug_assert!(askpass_delegate.is_some());
                    });
                    let response = client
                        .request(proto::Push {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            askpass_id,
                            branch_name: branch.to_string(),
                            remote_name: remote.to_string(),
                            options: options.map(|options| match options {
                                PushOptions::Force => proto::push::PushOptions::Force,
                                PushOptions::SetUpstream => proto::push::PushOptions::SetUpstream,
                            } as i32),
                        })
                        .await
                        .context("sending push request")?;

                    Ok(RemoteCommandOutput {
                        stdout: response.stdout,
                        stderr: response.stderr,
                    })
                }
            }
        })
    }

    pub fn pull(
        &mut self,
        branch: SharedString,
        remote: SharedString,
        askpass: AskPassDelegate,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let executor = cx.background_executor().clone();
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let env = self.worktree_environment(cx);

        self.send_job(move |git_repo, cx| async move {
            match git_repo {
                GitRepo::Local(git_repository) => {
                    let askpass = AskPassSession::new(&executor, askpass).await?;
                    let env = env.await;
                    git_repository
                        .pull(branch.to_string(), remote.to_string(), askpass, env, cx)
                        .await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    askpass_delegates.lock().insert(askpass_id, askpass);
                    let _defer = util::defer(|| {
                        let askpass_delegate = askpass_delegates.lock().remove(&askpass_id);
                        debug_assert!(askpass_delegate.is_some());
                    });
                    let response = client
                        .request(proto::Pull {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            askpass_id,
                            branch_name: branch.to_string(),
                            remote_name: remote.to_string(),
                        })
                        .await
                        .context("sending pull request")?;

                    Ok(RemoteCommandOutput {
                        stdout: response.stdout,
                        stderr: response.stderr,
                    })
                }
            }
        })
    }

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<String>,
        cx: &mut App,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let env = self.worktree_environment(cx);

        self.send_keyed_job(
            Some(GitJobKey::WriteIndex(path.clone())),
            |git_repo, cx| async move {
                match git_repo {
                    GitRepo::Local(repo) => repo.set_index_text(path, content, env.await, cx).await,
                    GitRepo::Remote {
                        project_id,
                        client,
                        worktree_id,
                        work_directory_id,
                    } => {
                        client
                            .request(proto::SetIndexText {
                                project_id: project_id.0,
                                worktree_id: worktree_id.to_proto(),
                                work_directory_id: work_directory_id.to_proto(),
                                path: path.as_ref().to_proto(),
                                text: content,
                            })
                            .await?;
                        Ok(())
                    }
                }
            },
        )
    }

    pub fn get_remotes(
        &self,
        branch_name: Option<String>,
    ) -> oneshot::Receiver<Result<Vec<Remote>>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => git_repository.get_remotes(branch_name, cx).await,
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    let response = client
                        .request(proto::GetRemotes {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            branch_name,
                        })
                        .await?;

                    let remotes = response
                        .remotes
                        .into_iter()
                        .map(|remotes| git::repository::Remote {
                            name: remotes.name.into(),
                        })
                        .collect();

                    Ok(remotes)
                }
            }
        })
    }

    pub fn branches(&self) -> oneshot::Receiver<Result<Vec<Branch>>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => {
                    let git_repository = git_repository.clone();
                    cx.background_spawn(async move { git_repository.branches().await })
                        .await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    let response = client
                        .request(proto::GitGetBranches {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                        })
                        .await?;

                    let branches = response
                        .branches
                        .into_iter()
                        .map(|branch| worktree::proto_to_branch(&branch))
                        .collect();

                    Ok(branches)
                }
            }
        })
    }

    pub fn diff(&self, diff_type: DiffType, _cx: &App) -> oneshot::Receiver<Result<String>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => git_repository.diff(diff_type, cx).await,
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                    ..
                } => {
                    let response = client
                        .request(proto::GitDiff {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            diff_type: match diff_type {
                                DiffType::HeadToIndex => {
                                    proto::git_diff::DiffType::HeadToIndex.into()
                                }
                                DiffType::HeadToWorktree => {
                                    proto::git_diff::DiffType::HeadToWorktree.into()
                                }
                            },
                        })
                        .await?;

                    Ok(response.diff)
                }
            }
        })
    }

    pub fn create_branch(&self, branch_name: String) -> oneshot::Receiver<Result<()>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => {
                    git_repository.create_branch(branch_name, cx).await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    client
                        .request(proto::GitCreateBranch {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            branch_name,
                        })
                        .await?;

                    Ok(())
                }
            }
        })
    }

    pub fn change_branch(&self, branch_name: String) -> oneshot::Receiver<Result<()>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => {
                    git_repository.change_branch(branch_name, cx).await
                }
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    client
                        .request(proto::GitChangeBranch {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                            branch_name,
                        })
                        .await?;

                    Ok(())
                }
            }
        })
    }

    pub fn check_for_pushed_commits(&self) -> oneshot::Receiver<Result<Vec<SharedString>>> {
        self.send_job(|repo, cx| async move {
            match repo {
                GitRepo::Local(git_repository) => git_repository.check_for_pushed_commit(cx).await,
                GitRepo::Remote {
                    project_id,
                    client,
                    worktree_id,
                    work_directory_id,
                } => {
                    let response = client
                        .request(proto::CheckForPushedCommits {
                            project_id: project_id.0,
                            worktree_id: worktree_id.to_proto(),
                            work_directory_id: work_directory_id.to_proto(),
                        })
                        .await?;

                    let branches = response.pushed_to.into_iter().map(Into::into).collect();

                    Ok(branches)
                }
            }
        })
    }
}
