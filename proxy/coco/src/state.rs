//! Utility to work with the peer api of librad.

use std::iter::FromIterator;
use std::ops::Deref;
use std::{convert::TryFrom as _, net::SocketAddr, path::PathBuf};

use either::Either;

use librad::{
    git::{
        identities,
        identities::local::{self, LocalIdentity},
        identities::person,
        identities::project,
        include::{self, Include},
        local::url::LocalUrl,
        refs::Refs,
        replication, storage, tracking,
        types::{namespace, Namespace, Reference, Single},
    },
    git_ext::{OneLevel, RefLike},
    identities::delegation::{Direct, Indirect},
    identities::payload,
    identities::{Person, Project, SomeIdentity, Urn},
    internal::canonical::Cstring,
    keys,
    net::peer::PeerApi,
    paths,
    peer::PeerId,
};
use radicle_keystore::sign::Signer as _;
use radicle_surf::vcs::{git, git::git2};

use crate::{
    peer::gossip,
    project::peer,
    seed::Seed,
    signer, source,
    user::{verify as verify_user, User},
};

pub mod error;
pub use error::Error;

/// High-level interface to the coco monorepo and gossip layer.
#[derive(Clone)]
pub struct State {
    /// Internal handle on [`PeerApi`].
    pub(crate) api: PeerApi,
    /// Signer to sign artifacts generated by the user.
    signer: signer::BoxedSigner,
}

impl State {
    /// Create a new [`State`] given a [`PeerApi`].
    #[must_use]
    pub fn new(api: PeerApi, signer: signer::BoxedSigner) -> Self {
        Self { api, signer }
    }

    /// Returns the [`PathBuf`] to the underlying monorepo.
    #[must_use]
    pub fn monorepo(&self) -> PathBuf {
        self.api.paths().git_dir().join("")
    }

    /// Returns the underlying [`paths::Paths`].
    #[must_use]
    pub fn paths(&self) -> paths::Paths {
        self.api.paths().clone()
    }

    /// Check the storage to see if we have the given commit for project at `urn`.
    ///
    /// # Errors
    ///
    ///   * Checking the storage for the commit fails.
    pub async fn has_commit<Oid>(&self, urn: Urn, oid: Oid) -> Result<bool, Error>
    where
        Oid: AsRef<git2::Oid> + std::fmt::Debug + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| storage.has_commit(&urn, oid))
            .await??)
    }

    /// The local machine's [`PeerId`].WEB
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.api.peer_id()
    }

    /// The [`SocketAddr`] this [`PeerApi`] is listening on.
    #[must_use]
    pub fn listen_addrs(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.api.listen_addrs()
    }

    /// Get the default owner for this `PeerApi`.
    pub async fn default_owner(&self) -> Result<Option<LocalIdentity>, Error> {
        self.api
            .with_storage(move |storage| {
                if let Some(urn) = storage.config()?.user()? {
                    return local::load(&storage, urn).map_err(Error::from);
                }

                Ok::<_, Error>(None)
            })
            .await?
    }

    /// Set the default owner for this `PeerApi`.
    ///
    /// # Errors
    ///
    ///   * Fails to set the default `rad/self` for this `PeerApi`.
    pub async fn set_default_owner<U>(&self, user: U) -> Result<(), Error>
    where
        U: Into<Option<LocalIdentity>> + Send + Sync + 'static,
    {
        self.api
            .with_storage(move |storage| storage.config()?.set_user(user).map_err(Error::from))
            .await?
    }

    /// Initialise a [`User`] and make them the default owner of this [`PeerApi`].
    ///
    /// # Errors
    ///
    ///   * Fails to initialise `User`.
    ///   * Fails to verify `User`.
    ///   * Fails to set the default `rad/self` for this `PeerApi`.
    pub async fn init_owner(&self, name: String) -> Result<LocalIdentity, Error> {
        match self
            .api
            .with_storage(move |store| local::default(&store))
            .await??
        {
            Some(owner) => Ok(owner),
            None => {
                let pk = keys::PublicKey::from(self.signer.public_key());
                let person = self
                    .api
                    .with_storage(move |store| {
                        person::create(
                            &store,
                            payload::Person {
                                name: Cstring::from(name),
                            },
                            Direct::from_iter(vec![pk].into_iter()),
                        )
                    })
                    .await??;

                let owner = self
                    .api
                    .with_storage(move |store| local::load(&store, person.urn()))
                    .await??
                    .unwrap();

                {
                    let owner = owner.clone();
                    self.api
                        .with_storage(move |store| store.config().unwrap().set_user(owner))
                        .await??;
                }

                Ok(owner)
            }
        }
    }

    /// Given some hints as to where you might find it, get the urn of the project found at `url`.
    ///
    /// # Errors
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to clone the project.
    ///   * Failed to set the rad/self of this project.
    pub async fn clone_project<Addrs>(
        &self,
        urn: Urn,
        remote_peer: PeerId,
        addr_hints: Addrs,
    ) -> Result<(), Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        self.api
            .with_storage(move |store| {
                replication::replicate(&store, None, urn.clone(), remote_peer, addr_hints)
            })
            .await?
            .map_err(Error::from)
    }

    /// Get the project found at `urn`.
    ///
    /// # Errors
    ///
    ///   * Resolving the project fails.
    pub async fn get_project<P>(&self, urn: Urn, peer: P) -> Result<Option<Project>, Error>
    where
        P: Into<Option<PeerId>> + Send + 'static,
    {
        self.api
            .with_storage(move |store| identities::project::get(&store, &urn))
            .await?
            .map_err(Error::from)
    }

    /// Returns the list of [`librad_project::Project`]s for the local peer.
    ///
    /// # Errors
    ///
    ///   * Retrieving the project entities from the store fails.
    pub async fn list_projects(&self) -> Result<Vec<Project>, Error> {
        self.api
            .with_storage(move |store| {
                let projects = identities::any::list(&store)?
                    .filter_map(Result::ok)
                    .filter_map(|id| match id {
                        SomeIdentity::Person(_person) => None,
                        SomeIdentity::Project(project) => Some(project),
                    })
                    .collect::<Vec<_>>();

                Ok::<_, Error>(projects)
            })
            .await?
    }

    /// Retrieves the [`librad::git::refs::Refs`] for the state owner.
    ///
    /// # Errors
    ///
    /// * if opening the storage fails
    pub async fn list_owner_project_refs(&self, urn: Urn) -> Result<Option<Refs>, Error> {
        self.api
            .with_storage(move |store| Refs::load(&store, &urn, None))
            .await?
            .map_err(Error::from)
    }

    /// Retrieves the [`librad::git::refs::Refs`] for the given project urn.
    ///
    /// # Errors
    ///
    /// * if opening the storage fails
    pub async fn list_peer_project_refs(
        &self,
        urn: Urn,
        peer_id: PeerId,
    ) -> Result<Option<Refs>, Error> {
        self.api
            .with_storage(move |store| Refs::load(&store, &urn, Some(peer_id)))
            .await?
            .map_err(Error::from)
    }

    /// Returns the list of [`user::User`]s known for your peer.
    ///
    /// # Errors
    ///
    ///   * Retrieval of the user entities from the store fails.
    pub async fn list_users(&self) -> Result<Vec<Person>, Error> {
        self.api
            .with_storage(move |store| {
                let projects = identities::any::list(&store)?
                    .filter_map(Result::ok)
                    .filter_map(|id| match id {
                        SomeIdentity::Person(person) => Some(person),
                        SomeIdentity::Project(_project) => None,
                    })
                    .collect::<Vec<_>>();

                Ok::<_, Error>(projects)
            })
            .await?
    }

    /// Given some hints as to where you might find it, get the urn of the user found at `url`.
    ///
    /// # Errors
    ///
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to clone the user.
    pub async fn clone_user<Addrs>(
        &self,
        urn: Urn,
        remote_peer: PeerId,
        addr_hints: Addrs,
    ) -> Result<(), Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        self.api
            .with_storage(move |store| {
                replication::replicate(&store, None, urn, remote_peer, addr_hints)
            })
            .await?
            .map_err(Error::from)
    }

    /// Get the user found at `urn`.
    ///
    /// # Errors
    ///
    ///   * Resolving the user fails.
    ///   * Could not successfully acquire a lock to the API.
    pub async fn get_user(&self, urn: Urn) -> Result<Option<Person>, Error> {
        self.api
            .with_storage(move |store| identities::person::get(&store, &urn))
            .await?
            .map_err(Error::from)
    }

    /// Fetch any updates at the given `RadUrl`, providing address hints if we have them.
    ///
    /// # Errors
    ///
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to fetch the updates.
    ///   * Failed to set the rad/self of this project.
    pub async fn fetch<Addrs>(
        &self,
        urn: Urn,
        remote_peer: PeerId,
        addr_hints: Addrs,
    ) -> Result<(), Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |store| {
                replication::replicate(&store, None, urn, remote_peer, addr_hints)
            })
            .await??)
    }

    /// Provide a a repo [`git::Browser`] where the `Browser` is initialised with the provided
    /// `reference`.
    ///
    /// See [`State::find_default_branch`] and [`State::get_branch`] for obtaining a
    /// [`NamespacedRef`].
    ///
    /// # Errors
    ///   * If the namespace of the reference could not be converted to a [`git::Namespace`].
    ///   * If we could not open the backing storage.
    ///   * If we could not initialise the `Browser`.
    ///   * If the callback provided returned an error.
    pub async fn with_browser<F, T, C>(
        &self,
        reference: Reference<Single>,
        callback: F,
    ) -> Result<T, Error>
    where
        F: FnOnce(&mut git::Browser) -> Result<T, source::Error> + Send,
    {
        // CONSTRUCT PROEJECTS NAMESPACE
        let namespace =
            git::namespace::Namespace::try_from(reference.namespace.unwrap().to_string().as_str())
                .unwrap();

        // HANDLE HEADS
        let branch = match reference.remote {
            None => git::Branch::local(reference.name.as_str()),
            Some(peer) => git::Branch::remote(
                &format!("heads/{}", reference.name.as_str()),
                &peer.to_string(),
            ),
        };

        // OPEN BROWSER
        let monorepo = self.monorepo();
        let repo = git::Repository::new(monorepo).map_err(source::Error::from)?;
        let mut browser = git::Browser::new_with_namespace(&repo, &namespace, branch)
            .map_err(source::Error::from)?;

        // CALL CALLBACK
        callback(&mut browser).map_err(Error::from)
    }

    /// This method helps us get a branch for a given [`Urn`] and optional [`PeerId`].
    ///
    /// If the `branch_name` is `None` then we get the project for the given [`Urn`] and use its
    /// `default_branch`.
    ///
    /// # Errors
    ///   * If the storage operations fail.
    ///   * If the requested reference was not found.
    pub async fn get_branch<P, B>(
        &self,
        urn: Urn,
        remote: P,
        branch_name: B,
    ) -> Result<Reference<Single>, Error>
    where
        P: Into<Option<PeerId>> + Clone + Send,
        B: Into<Option<Cstring>> + Clone + Send,
    {
        let name = match branch_name.into() {
            None => {
                let project = self.get_project(urn.clone(), None).await?.unwrap();
                project.subject().default_branch.clone().unwrap()
            }
            Some(name) => name,
        }
        .parse()?;

        let remote = match remote.into() {
            Some(peer_id) if peer_id == self.peer_id() => None,
            Some(peer_id) => Some(peer_id),
            None => None,
        };
        let reference = Reference::head(Namespace::from(urn), remote, name);
        let exists = {
            let reference = reference.clone();
            self.api
                .with_storage(move |storage| storage.has_ref(&reference))
                .await??
        };

        if exists {
            Ok(reference)
        } else {
            Err(Error::MissingRef { reference })
        }
    }

    /// This method helps us get the default branch for a given [`Urn`].
    ///
    /// It does this by:
    ///     * First checking if the owner of this storage has a reference to the default
    /// branch.
    ///     * If the owner does not have this reference then it falls back to the first maintainer.
    ///
    /// # Errors
    ///   * If the storage operations fail.
    ///   * If no default branch was found for the provided [`Urn`].
    pub async fn find_default_branch(&self, urn: Urn) -> Result<Reference<Single>, Error> {
        let project = self.get_project(urn.clone(), None).await?.unwrap();

        let owner = self.default_owner().await?.unwrap();

        let default_branch = project.subject().default_branch.clone().unwrap();

        // TODO(xla): Check for all delegations if there is default branch.
        let peer = project
            .delegations()
            .iter()
            .flat_map(|either| match either {
                Either::Left(pk) => Either::Left(std::iter::once(PeerId::from(*pk))),
                Either::Right(indirect) => {
                    Either::Right(indirect.delegations().iter().map(|pk| PeerId::from(*pk)))
                }
            })
            .next()
            .unwrap();

        let (owner, peer) = tokio::join!(
            self.get_branch(urn.clone(), None, default_branch.to_owned()),
            self.get_branch(urn.clone(), peer, default_branch.to_owned())
        );
        match owner.or(peer) {
            Ok(reference) => Ok(reference),
            Err(Error::MissingRef { .. }) => Err(Error::NoDefaultBranch {
                name: project.subject().name.to_string(),
                urn,
            }),
            Err(err) => Err(err),
        }
    }

    /// Initialize a [`librad_project::Project`] that is owned by the `owner`.
    /// This kicks off the history of the project, tracked by `librad`'s mono-repo.
    ///
    /// # Errors
    ///
    /// Will error if:
    ///     * The signing of the project metadata fails.
    ///     * The interaction with `librad` [`librad::git::storage::Storage`] fails.
    pub async fn init_project(
        &self,
        owner: LocalIdentity,
        create: crate::project::Create,
    ) -> Result<Project, Error> {
        let pk = keys::PublicKey::from(self.signer.public_key());
        let default_branch = create.default_branch.to_string();
        let description = create.description.to_string();
        let name = create.name.to_string();
        let project = self
            .api
            .with_storage(move |store| {
                project::create(
                    &store,
                    owner,
                    payload::Project {
                        default_branch: Some(Cstring::from(default_branch)),
                        description: Some(Cstring::from(description)),
                        name: Cstring::from(name),
                    },
                    Indirect::from(pk),
                )
            })
            .await??;
        log::debug!(
            "Created project '{}#{}'",
            project.urn(),
            project.subject().name
        );

        // TODO(xla): Validate project working copy before creation and don't depend on URL.
        let url = LocalUrl::from(project.urn());
        let repository = create
            .validate(url)
            .map_err(crate::project::create::Error::from)?;

        let repo = repository
            .setup_repo(
                project
                    .subject()
                    .description
                    .as_deref()
                    .unwrap_or(&String::default()),
            )
            .map_err(crate::project::create::Error::from)?;
        let include_path = self.update_include(project.urn()).await?;
        include::set_include_path(&repo, include_path)?;
        crate::peer::gossip::announce(self, &project.urn(), None).await;

        Ok(project)
    }

    /// Create a [`user::User`] with the provided `handle`. This assumes that you are creating a
    /// user that uses the secret key the `PeerApi` was configured with.
    ///
    /// # Errors
    ///
    /// Will error if:
    ///     * The signing of the user metadata fails.
    ///     * The interaction with `librad` [`librad::git::storage::Storage`] fails.
    pub async fn init_user(&self, name: String) -> Result<Person, Error> {
        let pk = keys::PublicKey::from(self.signer.public_key());
        self.api
            .with_storage(move |store| {
                person::create(
                    &store,
                    payload::Person {
                        name: Cstring::from(name),
                    },
                    Direct::from_iter(vec![pk].into_iter()),
                )
            })
            .await?
            .map_err(Error::from)
    }

    /// Wrapper around the storage track.
    ///
    /// # Errors
    ///
    /// * When the storage operation fails.
    pub async fn track(&self, urn: Urn, remote_peer: PeerId) -> Result<(), Error> {
        {
            let urn = urn.clone();
            self.api
                .with_storage(move |store| tracking::track(&store, &urn, remote_peer))
                .await??;
        }

        gossip::query(self, urn.clone(), Some(remote_peer)).await;
        let path = self.update_include(urn).await?;
        log::debug!("Updated include path @ `{}`", path.display());
        Ok(())
    }

    /// Wrapper around the storage untrack.
    ///
    /// # Errors
    ///
    /// * When the storage operation fails.
    pub async fn untrack(&self, urn: Urn, remote_peer: PeerId) -> Result<bool, Error> {
        let res = {
            let urn = urn.clone();
            self.api
                .with_storage(move |store| tracking::untrack(&store, &urn, remote_peer))
                .await??
        };

        // Only need to update if we did untrack an existing peer
        if res {
            let path = self.update_include(urn).await?;
            log::debug!("Updated include path @ `{}`", path.display());
        }
        Ok(res)
    }

    /// Get the [`user::User`]s that are tracking this project, including their [`PeerId`].
    ///
    /// # Errors
    ///
    /// * If we could not acquire the lock
    /// * If we could not open the storage
    /// * If did not have the `urn` in storage
    /// * If we could not fetch the tracked peers
    /// * If we could not get the `rad/self` of the peer
    pub async fn tracked(
        &self,
        urn: Urn,
    ) -> Result<Vec<crate::project::Peer<peer::Status<Person>>>, Error> {
        let project = self.get_project(urn.clone(), None).await?.unwrap();

        self.api
            .with_storage(move |store| {
                let mut peers = vec![];

                for peer_id in tracking::tracked(&store, &urn)? {
                    let ref_self = Reference::rad_self(Namespace::from(urn.clone()), peer_id);
                    let status = if store.has_ref(&ref_self)? {
                        let malkovich_urn = Urn::try_from(ref_self).unwrap();
                        let malkovich = person::get(&store, &malkovich_urn)?.unwrap();

                        if project
                            .delegations()
                            .owner(peer_id.as_public_key())
                            .is_some()
                        {
                            peer::Status::replicated(peer::Role::Maintainer, malkovich)
                        } else if store.has_ref(&Reference::rad_signed_refs(
                            Namespace::from(urn.clone()),
                            peer_id,
                        ))? {
                            peer::Status::replicated(peer::Role::Contributor, malkovich)
                        } else {
                            peer::Status::replicated(peer::Role::Tracker, malkovich)
                        }
                    } else {
                        peer::Status::NotReplicated
                    };

                    peers.push(crate::project::Peer::Remote { peer_id, status });
                }

                Ok::<_, Error>(peers)
            })
            .await?
    }

    // TODO(xla): Account for projects not replicated but wanted.
    /// Constructs the list of [`project::Peer`] for the given `urn`. The basis is the list of
    /// tracking peers of the project combined with the local view.
    ///
    /// # Errors
    ///
    /// * if the project is not present in the monorepo
    /// * if the retrieval of tracking peers fails
    ///
    /// # Panics
    ///
    /// * if the default owner can't be fetched
    pub async fn list_project_peers(
        &self,
        urn: Urn,
    ) -> Result<Vec<crate::project::Peer<peer::Status<Person>>>, Error> {
        let project = self.get_project(urn.clone(), None).await?.unwrap();

        let mut peers = vec![];

        let owner = self
            .default_owner()
            .await
            .expect("unable to find state owner")
            .unwrap()
            .into_inner()
            .into_inner();

        // CHECK IF OWNER IS MAINTAINER
        let status = if project
            .delegations()
            .owner(self.peer_id().as_public_key())
            .is_some()
        {
            peer::Status::replicated(peer::Role::Maintainer, owner)
        // CHECK IF OWNER IS CONTRIBUTOR
        } else if self
            .api
            .with_storage(move |store| {
                store.has_ref(&Reference::rad_signed_refs(
                    Namespace::from(project.urn().clone()),
                    None,
                ))
            })
            .await??
        {
            peer::Status::replicated(peer::Role::Contributor, owner)
        // CHECK IF OWNER IS TRACKER
        } else {
            peer::Status::replicated(peer::Role::Tracker, owner)
        };

        peers.push(crate::project::Peer::Local {
            peer_id: self.peer_id(),
            status,
        });

        let mut remotes = self.tracked(urn).await?;

        peers.append(&mut remotes);

        Ok(peers)
    }

    /// Creates a working copy for the project of the given `urn`.
    ///
    /// The `destination` is the directory where the caller wishes to place the working copy.
    ///
    /// The `peer_id` is from which peer we wish to base our checkout from.
    ///
    /// # Errors
    ///
    /// * if the project can't be found
    /// * if the include file creation fails
    /// * if the clone of the working copy fails
    pub async fn checkout<P>(
        &self,
        urn: Urn,
        peer_id: P,
        destination: PathBuf,
    ) -> Result<PathBuf, Error>
    where
        P: Into<Option<PeerId>> + Send + 'static,
    {
        let peer_id = peer_id.into();
        let proj = self.get_project(urn.clone(), peer_id).await?;
        let include_path = self.update_include(urn.clone()).await?;
        let default_branch: OneLevel = OneLevel::from(proj.default_branch().parse::<RefLike>()?);
        let checkout = crate::project::Checkout {
            urn: proj.urn(),
            name: proj.name().to_string(),
            default_branch,
            path: destination,
            include_path,
        };

        let ownership = match peer_id {
            None => crate::project::checkout::Ownership::Local(self.peer_id()),
            Some(remote) => {
                let handle = {
                    self.api
                        .with_storage(move |storage| {
                            let rad_self = storage.get_rad_self_of(&urn, remote)?;
                            Ok::<_, Error>(rad_self.name().to_string())
                        })
                        .await??
                };
                crate::project::checkout::Ownership::Remote {
                    handle,
                    remote,
                    local: self.peer_id(),
                }
            }
        };

        let path = {
            let results = self.transport_results();
            let path =
                tokio::task::spawn_blocking(move || checkout.run(ownership).map_err(Error::from))
                    .await
                    .expect("blocking checkout failed")?;

            Self::process_transport_results(&results)?;
            path
        };

        Ok(path)
    }

    /// Prepare the include file for the given `project` with the latest tracked peers.
    ///
    /// # Errors
    ///
    /// * if getting the list of tracked peers fails
    pub async fn update_include(&self, urn: Urn) -> Result<PathBuf, Error> {
        let local_url = LocalUrl::from_urn(urn.clone(), self.peer_id());
        let tracked = self.tracked(urn).await?;
        let include = Include::from_tracked_users(
            self.paths().git_includes_dir().to_path_buf(),
            local_url,
            tracked.into_iter().filter_map(|peer| {
                crate::project::Peer::replicated_remote(peer).map(|(p, u)| (u, p))
            }),
        )?;
        let include_path = include.file_path();
        log::info!("creating include file @ '{:?}'", include_path);
        include.save()?;

        Ok(include_path)
    }
}

impl From<&State> for Seed {
    fn from(state: &State) -> Self {
        Self {
            peer_id: state.peer_id(),
            addr: state.listen_addr(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod test {
    use std::{env, path::PathBuf};

    use librad::{git::storage, git_ext::OneLevel, keys::SecretKey, reflike};

    use crate::{config, control, project, signer};

    use super::{Error, State};

    fn fakie_project(path: PathBuf) -> project::Create {
        project::Create {
            repo: project::Repo::New {
                path,
                name: "fakie-nose-kickflip-backside-180-to-handplant".to_string(),
            },
            description: "rad git tricks".to_string(),
            default_branch: OneLevel::from(reflike!("dope")),
        }
    }

    fn radicle_project(path: PathBuf) -> project::Create {
        project::Create {
            repo: project::Repo::New {
                path,
                name: "radicalise".to_string(),
            },
            description: "the people".to_string(),
            default_branch: OneLevel::from(reflike!("power")),
        }
    }

    #[tokio::test]
    async fn can_create_user() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let annie = state.init_user("annie_are_you_ok?").await;
        assert!(annie.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn can_create_project() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        env::set_var("RAD_HOME", tmp_dir.path());
        let repo_path = tmp_dir.path().join("radicle");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project = state
            .init_project(&user, radicle_project(repo_path.clone()))
            .await;

        assert!(project.is_ok());
        assert!(repo_path.join("radicalise").exists());

        Ok(())
    }

    #[tokio::test]
    async fn can_create_project_for_existing_repo() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");
        let repo_path = repo_path.join("radicalise");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project = state
            .init_project(&user, radicle_project(repo_path.clone()))
            .await;

        assert!(project.is_ok());
        assert!(repo_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn cannot_create_user_twice() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let err = state.init_user("cloudhead").await;

        if let Err(Error::Storage(storage::Error::AlreadyExists(urn))) = err {
            assert_eq!(urn, user.urn())
        } else {
            panic!(
                "unexpected error when creating the user a second time: {:?}",
                err
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn cannot_create_project_twice() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project_creation = radicle_project(repo_path.clone());
        let project = state.init_project(&user, project_creation.clone()).await?;

        let err = state
            .init_project(&user, project_creation.into_existing())
            .await;

        if let Err(Error::Storage(storage::Error::AlreadyExists(urn))) = err {
            assert_eq!(urn, project.urn())
        } else {
            panic!(
                "unexpected error when creating the project a second time: {:?}",
                err
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn list_projects() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");

        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;

        control::setup_fixtures(&state, &user)
            .await
            .expect("unable to setup fixtures");

        let kalt = state.init_user("kalt").await?;
        let kalt = super::verify_user(kalt)?;
        let fakie = state.init_project(&kalt, fakie_project(repo_path)).await?;

        let projects = state.list_projects().await?;
        let mut project_names = projects
            .into_iter()
            .map(|project| project.name().to_string())
            .collect::<Vec<_>>();
        project_names.sort();

        assert_eq!(
            project_names,
            vec!["Monadic", "monokel", "open source coin", "radicle"]
        );

        assert!(!project_names.contains(&fakie.name().to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn list_users() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let cloudhead = state.init_user("cloudhead").await?;
        let _cloudhead = super::verify_user(cloudhead)?;
        let kalt = state.init_user("kalt").await?;
        let _kalt = super::verify_user(kalt)?;

        let users = state.list_users().await?;
        let mut user_handles = users
            .into_iter()
            .map(|user| user.name().to_string())
            .collect::<Vec<_>>();
        user_handles.sort();

        assert_eq!(user_handles, vec!["cloudhead", "kalt"],);

        Ok(())
    }
}
