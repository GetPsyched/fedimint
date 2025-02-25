//! # Client library for fedimintd
//!
//! This library provides a client interface to build module clients that can be
//! plugged together into a fedimint client that exposes a high-level interface
//! for application authors to integrate with.
//!
//! ## Module Clients
//! Module clients have to at least implement the [`module::ClientModule`] trait
//! and a factory struct implementing [`module::gen::ClientModuleGen`]. The
//! `ClientModule` trait defines the module types (tx inputs, outputs, etc.) as
//! well as the module's [state machines](sm::State).
//!
//! ### State machines
//! State machines are spawned when starting operations and drive them
//! forward in the background. All module state machines are run by a central
//! [`sm::Executor`]. This means typically starting an operation shall return
//! instantly.
//!
//! For example when doing a deposit the function starting it would immediately
//! return a deposit address and a [`OperationId`] (important concept, highly
//! recommended to read the docs) while spawning a state machine checking the
//! blockchain for incoming bitcoin transactions. The progress of these state
//! machines can then be *observed* using the operation id, but no further user
//! interaction is required to drive them forward.
//!
//! ### State Machine Contexts
//! State machines have access to both a [global context](sm::GlobalContext) as
//! well as to a [module-specific context](module::ClientModule::context).
//!
//! The global context provides access to the federation API and allows to claim
//! module outputs (and transferring the value into the client's wallet), which
//! can be used for refunds.
//!
//! The client-specific context can be used for other purposes, such as
//! supplying config to the state transitions or giving access to other APIs
//! (e.g. LN gateway in case of the lightning module).
//!
//! ### Extension traits
//! The modules themselves can only create inputs and outputs that then have to
//! be combined into transactions by the user and submitted via
//! [`Client::finalize_and_submit_transaction`]. To make this easier most module
//! client implementations contain an extension trait which is implemented for
//! [`Client`] and allows to create the most typical fedimint transactions with
//! a single function call.
//!
//! To observe the progress each high level operation function should be
//! accompanied by one returning a stream of high-level operation updates.
//! Internally that stream queries the state machines belonging to the
//! operation to determine the high-level operation state.
//!
//! ### Primary Modules
//! Not all modules have the ability to hold money for long. E.g. the lightning
//! module and its smart contracts are only used to incentivize LN payments, not
//! to hold money. The mint module on the other hand holds e-cash note and can
//! thus be used to fund transactions and to absorb change. Module clients with
//! this ability should implement [`ClientModule::supports_being_primary`] and
//! related methods.
//!
//! For a example of a client module see [the mint client](https://github.com/fedimint/fedimint/blob/master/modules/fedimint-mint-client/src/lib.rs).
//!
//! ## Client
//! The [`Client`] struct is the main entry point for application authors. It is
//! constructed using its builder which can be obtained via [`Client::builder`].
//! The supported module clients have to be chosen at compile time while the
//! actually available ones will be determined by the config loaded at runtime.
//!
//! For a hacky instantiation of a complete client see the [`ng` subcommand of `fedimint-cli`](https://github.com/fedimint/fedimint/blob/55f9d88e17d914b92a7018de677d16e57ed42bf6/fedimint-cli/src/ng.rs#L56-L73).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::io::{Error, Read, Write};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_stream::stream;
use fedimint_core::api::{
    ApiVersionSet, DynGlobalApi, DynModuleApi, GlobalFederationApi, IGlobalFederationApi,
    WsFederationApi,
};
use fedimint_core::config::{ClientConfig, FederationId, ModuleGenRegistry};
use fedimint_core::core::{DynInput, DynOutput, IInput, IOutput, ModuleInstanceId, ModuleKind};
use fedimint_core::db::{AutocommitError, Database, DatabaseTransaction, IDatabase};
use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::module::{
    ApiVersion, MultiApiVersion, SupportedApiVersionsSummary, SupportedCoreApiVersions,
    SupportedModuleApiVersions,
};
use fedimint_core::task::{MaybeSend, MaybeSync, TaskGroup};
use fedimint_core::time::now;
use fedimint_core::transaction::Transaction;
use fedimint_core::util::{BoxStream, NextOrPending};
use fedimint_core::{
    apply, async_trait_maybe_send, dyn_newtype_define, maybe_add_send_sync, Amount, OutPoint,
    TransactionId,
};
pub use fedimint_derive_secret as derivable_secret;
use fedimint_derive_secret::DerivableSecret;
use futures::StreamExt;
use module::DynClientModule;
use rand::thread_rng;
use secp256k1_zkp::{PublicKey, Secp256k1};
use secret::DeriveableSecretClientExt;
use serde::Serialize;
use tracing::info;

use crate::backup::Metadata;
use crate::db::ClientSecretKey;
use crate::module::gen::{
    ClientModuleGen, ClientModuleGenRegistry, DynClientModuleGen, IClientModuleGen,
};
use crate::module::{ClientModule, ClientModuleRegistry, IClientModule, StateGenerator};
use crate::oplog::OperationLog;
use crate::secret::RootSecretStrategy;
use crate::sm::executor::{
    ActiveOperationStateKeyPrefix, ContextGen, InactiveOperationStateKeyPrefix,
};
use crate::sm::{
    ClientSMDatabaseTransaction, DynState, Executor, GlobalContext, IState, Notifier, OperationId,
    OperationState, State,
};
use crate::transaction::{
    tx_submission_sm_decoder, ClientInput, ClientOutput, TransactionBuilder,
    TransactionBuilderBalance, TxSubmissionContext, TxSubmissionError, TxSubmissionStates,
    TRANSACTION_SUBMISSION_MODULE_INSTANCE,
};

/// Client backup
pub mod backup;
/// Database keys used by the client
pub mod db;
/// Module client interface definitions
pub mod module;
/// Operation log subsystem of the client
pub mod oplog;
/// Secret handling & derivation
pub mod secret;
/// Client state machine interfaces and executor implementation
pub mod sm;
/// Structs and interfaces to construct Fedimint transactions
pub mod transaction;

pub type InstancelessDynClientInput = ClientInput<
    Box<maybe_add_send_sync!(dyn IInput + 'static)>,
    Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext> + 'static)>,
>;

pub type InstancelessDynClientOutput = ClientOutput<
    Box<maybe_add_send_sync!(dyn IOutput + 'static)>,
    Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext> + 'static)>,
>;

#[apply(async_trait_maybe_send!)]
pub trait IGlobalClientContext: Debug + MaybeSend + MaybeSync + 'static {
    /// Returned a reference client's module API client, so that module-specific
    /// calls can be made
    fn module_api(&self) -> DynModuleApi;

    fn client_config(&self) -> &ClientConfig;

    /// Returns a reference to the client's federation API client. The provided
    /// interface [`IGlobalFederationApi`] typically does not provide the
    /// necessary functionality, for this extension traits like
    /// [`fedimint_core::api::GlobalFederationApi`] have to be used.
    // TODO: Could be removed in favor of client() except for testing
    fn api(&self) -> &DynGlobalApi;

    fn decoders(&self) -> &ModuleDecoderRegistry;

    /// This function is mostly meant for internal use, you are probably looking
    /// for [`DynGlobalClientContext::claim_input`].
    /// Returns transaction id of the funding transaction and an optional
    /// `OutPoint` that represents change if change was added.
    async fn claim_input_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        input: InstancelessDynClientInput,
    ) -> (TransactionId, Option<OutPoint>);

    /// This function is mostly meant for internal use, you are probably looking
    /// for [`DynGlobalClientContext::fund_output`].
    /// Returns transaction id of the funding transaction and an optional
    /// `OutPoint` that represents change if change was added.
    async fn fund_output_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        output: InstancelessDynClientOutput,
    ) -> anyhow::Result<(TransactionId, Option<OutPoint>)>;

    /// Adds a state machine to the executor.
    async fn add_state_machine_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        sm: Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext>)>,
    ) -> anyhow::Result<()>;

    async fn transaction_update_stream(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<OperationState<TxSubmissionStates>>;
}

dyn_newtype_define! {
    /// Global state and functionality provided to all state machines running in the
    /// client
    #[derive(Clone)]
    pub DynGlobalClientContext(Arc<IGlobalClientContext>)
}

impl DynGlobalClientContext {
    // TODO: Remove in favor of `await_tx_accepted`
    pub async fn await_tx_rejected(&self, operation_id: OperationId, txid: TransactionId) {
        let update_stream = self.transaction_update_stream(operation_id).await;

        let query_txid = txid;
        update_stream
            .filter(move |tx_submission_state| {
                std::future::ready(matches!(
                    tx_submission_state.state,
                    TxSubmissionStates::Rejected { txid, .. } if txid == query_txid
                ))
            })
            .next_or_pending()
            .await;
    }

    pub async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        txid: TransactionId,
    ) -> Result<(), TxSubmissionError> {
        let update_stream = self.transaction_update_stream(operation_id).await;

        let query_txid = txid;
        update_stream
            .filter_map(|tx_update| {
                std::future::ready(match tx_update.state {
                    TxSubmissionStates::Accepted { txid } if txid == query_txid => Some(Ok(())),
                    TxSubmissionStates::Rejected { txid, error } if txid == query_txid => {
                        Some(Err(error))
                    }
                    _ => None,
                })
            })
            .next_or_pending()
            .await
    }

    /// Creates a transaction that with an output of the primary module,
    /// claiming the given input and transferring its value into the client's
    /// wallet.
    ///
    /// The transactions submission state machine as well as the state
    /// machines responsible for the generated output are generated
    /// automatically. The caller is responsible for the input's state machines,
    /// should there be any required.
    pub async fn claim_input<I, S>(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        input: ClientInput<I, S>,
    ) -> (TransactionId, Option<OutPoint>)
    where
        I: IInput + MaybeSend + MaybeSync + 'static,
        S: IState<DynGlobalClientContext> + MaybeSend + MaybeSync + 'static,
    {
        self.claim_input_dyn(
            dbtx,
            InstancelessDynClientInput {
                input: Box::new(input.input),
                keys: input.keys,
                state_machines: states_to_instanceless_dyn(input.state_machines),
            },
        )
        .await
    }

    /// Creates a transaction with the supplied output and funding added by the
    /// primary module if possible. If the primary module does not have the
    /// required funds this function fails.
    ///
    /// The transactions submission state machine as well as the state machines
    /// for the funding inputs are generated automatically. The caller is
    /// responsible for the output's state machines, should there be any
    /// required.
    pub async fn fund_output<O, S>(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        output: ClientOutput<O, S>,
    ) -> anyhow::Result<(TransactionId, Option<OutPoint>)>
    where
        O: IOutput + MaybeSend + MaybeSync + 'static,
        S: IState<DynGlobalClientContext> + MaybeSend + MaybeSync + 'static,
    {
        self.fund_output_dyn(
            dbtx,
            InstancelessDynClientOutput {
                output: Box::new(output.output),
                state_machines: states_to_instanceless_dyn(output.state_machines),
            },
        )
        .await
    }

    /// Allows adding state machines from inside a transition to the executor.
    /// The added state machine belongs to the same module instance as the state
    /// machine from inside which it was spawned.
    pub async fn add_state_machine<S>(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        sm: S,
    ) -> anyhow::Result<()>
    where
        S: State<GlobalContext = DynGlobalClientContext> + MaybeSend + MaybeSync + 'static,
    {
        self.add_state_machine_dyn(dbtx, box_up_state(sm)).await
    }
}

fn states_to_instanceless_dyn<
    S: IState<DynGlobalClientContext> + MaybeSend + MaybeSync + 'static,
>(
    state_gen: StateGenerator<S>,
) -> StateGenerator<Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext> + 'static)>> {
    Arc::new(move |txid, out_idx| {
        let states: Vec<S> = state_gen(txid, out_idx);
        states
            .into_iter()
            .map(|state| box_up_state(state))
            .collect()
    })
}

/// Not sure why I couldn't just directly call `Box::new` ins
/// [`states_to_instanceless_dyn`], but this fixed it.
fn box_up_state(
    state: impl IState<DynGlobalClientContext> + 'static,
) -> Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext> + 'static)> {
    Box::new(state)
}

impl<T> From<Arc<T>> for DynGlobalClientContext
where
    T: IGlobalClientContext,
{
    fn from(inner: Arc<T>) -> Self {
        DynGlobalClientContext(inner)
    }
}

impl GlobalContext for DynGlobalClientContext {}

// TODO: impl `Debug` for `Client` and derive here
impl Debug for ClientInner {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClientInner")
    }
}

/// Global state given to a specific client module and state. It is aware inside
/// which module instance and operation it is used and to avoid module being
/// aware of their instance id etc.
#[derive(Clone, Debug)]
struct ModuleGlobalClientContext {
    client: Arc<ClientInner>,
    module_instance_id: ModuleInstanceId,
    operation: OperationId,
}

#[apply(async_trait_maybe_send!)]
impl IGlobalClientContext for ModuleGlobalClientContext {
    fn module_api(&self) -> DynModuleApi {
        self.api().with_module(self.module_instance_id)
    }

    fn api(&self) -> &DynGlobalApi {
        &self.client.api
    }

    fn decoders(&self) -> &ModuleDecoderRegistry {
        self.client.decoders()
    }

    fn client_config(&self) -> &ClientConfig {
        self.client.config()
    }

    async fn claim_input_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        input: InstancelessDynClientInput,
    ) -> (TransactionId, Option<OutPoint>) {
        let instance_input = ClientInput {
            input: DynInput::from_parts(self.module_instance_id, input.input),
            keys: input.keys,
            state_machines: states_add_instance(self.module_instance_id, input.state_machines),
        };

        self.client
            .finalize_and_submit_transaction(
                dbtx.global_tx(),
                self.operation,
                TransactionBuilder::new().with_input(instance_input),
            )
            .await
            .expect("Can obly fail if additional funding is needed")
    }

    async fn fund_output_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        output: InstancelessDynClientOutput,
    ) -> anyhow::Result<(TransactionId, Option<OutPoint>)> {
        let instance_output = ClientOutput {
            output: DynOutput::from_parts(self.module_instance_id, output.output),
            state_machines: states_add_instance(self.module_instance_id, output.state_machines),
        };

        self.client
            .finalize_and_submit_transaction(
                dbtx.global_tx(),
                self.operation,
                TransactionBuilder::new().with_output(instance_output),
            )
            .await
    }

    async fn add_state_machine_dyn(
        &self,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        sm: Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext>)>,
    ) -> anyhow::Result<()> {
        let state = DynState::from_parts(self.module_instance_id, sm);

        self.client
            .executor
            .add_state_machines_dbtx(dbtx.global_tx(), vec![state])
            .await
    }

    async fn transaction_update_stream(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<OperationState<TxSubmissionStates>> {
        self.client.transaction_update_stream(operation_id).await
    }
}

fn states_add_instance(
    module_instance_id: ModuleInstanceId,
    state_gen: StateGenerator<
        Box<maybe_add_send_sync!(dyn IState<DynGlobalClientContext> + 'static)>,
    >,
) -> StateGenerator<DynState<DynGlobalClientContext>> {
    Arc::new(move |txid, out_idx| {
        let states = state_gen(txid, out_idx);
        Iterator::collect(
            states
                .into_iter()
                .map(|state| DynState::from_parts(module_instance_id, state)),
        )
    })
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

/// List of core api versions supported by the implementation.
/// Notably `major` version is the one being supported, and corresponding
/// `minor` version is the one required (for given `major` version).
const SUPPORTED_CORE_API_VERSIONS: &[fedimint_core::module::ApiVersion] =
    &[ApiVersion { major: 0, minor: 0 }];

pub type ModuleGlobalContextGen = ContextGen<DynGlobalClientContext>;

impl Client {
    /// Initialize a client builder that can be configured to create a new
    /// client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub async fn start_executor(&self, tg: &mut TaskGroup) {
        self.inner
            .executor
            .start_executor(tg, self.inner.context_gen())
            .await
    }

    pub fn api(&self) -> &(dyn IGlobalFederationApi + 'static) {
        self.inner.api.as_ref()
    }

    pub fn federation_id(&self) -> FederationId {
        self.inner.federation_id
    }

    pub fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)> {
        Ok((
            self.federation_id()
                .to_fake_ln_pub_key(&self.inner.secp_ctx)?,
            0,
        ))
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.inner.federation_meta.get(key).cloned()
    }

    pub fn decoders(&self) -> &ModuleDecoderRegistry {
        self.inner.decoders()
    }

    fn root_secret(&self) -> DerivableSecret {
        self.inner.root_secret.clone()
    }

    /// Add funding and/or change to the transaction builder as needed, finalize
    /// the transaction and submit it to the federation.
    pub async fn finalize_and_submit_transaction<F, M>(
        &self,
        operation_id: OperationId,
        operation_type: &str,
        operation_meta: F,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId>
    where
        F: Fn(TransactionId, Option<OutPoint>) -> M + Clone + MaybeSend + MaybeSync,
        M: serde::Serialize + MaybeSend,
    {
        let operation_type = operation_type.to_owned();

        let autocommit_res = self
            .inner
            .db
            .autocommit(
                |dbtx| {
                    let operation_type = operation_type.clone();
                    let tx_builder = tx_builder.clone();
                    let operation_meta = operation_meta.clone();
                    Box::pin(async move {
                        if ClientInner::operation_exists(dbtx, operation_id).await {
                            bail!("There already exists an operation with id {operation_id:?}")
                        }

                        let (txid, change_outpoint) = self
                            .inner
                            .finalize_and_submit_transaction(dbtx, operation_id, tx_builder)
                            .await?;

                        self.operation_log()
                            .add_operation_log_entry(
                                dbtx,
                                operation_id,
                                &operation_type,
                                operation_meta(txid, change_outpoint),
                            )
                            .await;

                        Ok(txid)
                    })
                },
                Some(100), // TODO: handle what happens after 100 retries
            )
            .await;

        match autocommit_res {
            Ok(txid) => Ok(txid),
            Err(AutocommitError::ClosureError { error, .. }) => Err(error),
            Err(AutocommitError::CommitFailed {
                attempts,
                last_error,
            }) => panic!(
                "Failed to commit tx submission dbtx after {attempts} attempts: {last_error}"
            ),
        }
    }

    pub async fn add_state_machines(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        states: Vec<DynState<DynGlobalClientContext>>,
    ) -> anyhow::Result<()> {
        self.inner
            .executor
            .add_state_machines_dbtx(dbtx, states)
            .await
    }

    // TODO: implement as part of OperationLog
    pub async fn get_active_operations(&self) -> HashSet<OperationId> {
        self.inner.executor.get_active_operations().await
    }

    pub fn operation_log(&self) -> &OperationLog {
        &self.inner.operation_log
    }

    /// Returns a reference to a typed module client instance by kind
    pub fn get_first_module<M: ClientModule>(
        &self,
        module_kind: &ModuleKind,
    ) -> (&M, ClientModuleInstance) {
        let id = self
            .get_first_instance(module_kind)
            .unwrap_or_else(|| panic!("No modules found of kind {module_kind}"));
        let module: &M = self
            .inner
            .try_get_module(id)
            .unwrap_or_else(|| panic!("Unknown module instance {id}"))
            .as_any()
            .downcast_ref::<M>()
            .unwrap_or_else(|| panic!("Module is not of type {}", std::any::type_name::<M>()));
        let instance = ClientModuleInstance {
            id,
            db: self.db().new_isolated(id),
            api: self.api().with_module(id),
        };
        (module, instance)
    }

    pub fn get_module_client_dyn(
        &self,
        instance_id: ModuleInstanceId,
    ) -> anyhow::Result<&maybe_add_send_sync!(dyn IClientModule)> {
        self.inner
            .try_get_module(instance_id)
            .ok_or(anyhow!("Unknown module instance {}", instance_id))
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// Returns a stream of transaction updates for the given operation id that
    /// can later be used to watch for a specific transaction being accepted.
    pub async fn transaction_updates(&self, operation_id: OperationId) -> TransactionUpdates {
        TransactionUpdates {
            update_stream: self.inner.transaction_update_stream(operation_id).await,
        }
    }

    /// Returns the instance id of the first module of the given kind. The
    /// primary module will always be returned before any other modules (which
    /// themselves are ordered by their instance id).
    pub fn get_first_instance(&self, module_kind: &ModuleKind) -> Option<ModuleInstanceId> {
        if &self
            .inner
            .modules
            .get_with_kind(self.inner.primary_module_instance)
            .expect("must have primary module")
            .0
            == module_kind
        {
            return Some(self.inner.primary_module_instance);
        }

        self.inner
            .modules
            .iter_modules()
            .find(|(_, kind, _module)| *kind == module_kind)
            .map(|(instance_id, _, _)| instance_id)
    }

    /// Returns the data from which the client's root secret is derived (e.g.
    /// BIP39 seed phrase struct).
    pub async fn root_secret_encoding<S>(&self) -> S::Encoding
    where
        S: RootSecretStrategy,
    {
        get_client_root_secret_encoding::<S>(self.db()).await
    }

    /// Waits for an output from the primary module to reach its final
    /// state.
    pub async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        out_point: OutPoint,
    ) -> anyhow::Result<Amount> {
        self.inner
            .await_primary_module_output(operation_id, out_point)
            .await
    }

    /// Returns the config with which the client was initialized.
    pub async fn get_config(&self) -> &ClientConfig {
        &self.inner.config
    }

    /// Get the primary module
    pub fn primary_module(&self) -> &DynClientModule {
        self.inner
            .modules
            .get(self.inner.primary_module_instance)
            .expect("primary module must be present")
    }

    /// Balance available to the client for spending
    pub async fn get_balance(&self) -> Amount {
        self.primary_module()
            .get_balance(
                self.inner.primary_module_instance,
                &mut self.db().begin_transaction().await,
            )
            .await
    }

    /// Returns a stream that yields the current client balance every time it
    /// changes.
    pub async fn subscribe_balance_changes(&self) -> BoxStream<'_, Amount> {
        let mut balance_changes = self.primary_module().subscribe_balance_changes().await;
        Box::pin(stream! {
            while let Some(()) = balance_changes.next().await {
                let mut dbtx = self.db().begin_transaction().await;
                let balance = self
                    .primary_module()
                    .get_balance(self.inner.primary_module_instance, &mut dbtx)
                    .await;
                yield balance;
            }
        })
    }

    /// Query the federation for API version support and then calculate
    /// the best API version to use (supported by most guardians).
    pub async fn discover_common_api_version(&self) -> anyhow::Result<ApiVersionSet> {
        Ok(self
            .api()
            .discover_api_version_set(&self.supported_api_versions_summary().await)
            .await?)
    }

    /// [`SupportedApiVersionsSummary`] that the client and its modules support
    pub async fn supported_api_versions_summary(&self) -> SupportedApiVersionsSummary {
        let config = self.get_config().await;

        SupportedApiVersionsSummary {
            core: SupportedCoreApiVersions {
                core_consensus: config.consensus_version,
                api: MultiApiVersion::try_from_iter(SUPPORTED_CORE_API_VERSIONS.to_owned())
                    .expect("must not have conflicting versions"),
            },
            modules: self
                .inner
                .modules
                .iter_modules()
                .filter_map(|(module_instance_id, _, module)| {
                    config
                        .modules
                        .get(&module_instance_id)
                        .map(|module_config| {
                            (
                                module_instance_id,
                                SupportedModuleApiVersions {
                                    core_consensus: config.consensus_version,
                                    module_consensus: module_config.version,
                                    api: module.supported_api_versions(),
                                },
                            )
                        })
                })
                .collect(),
        }
    }
}

/// Resources particular to a module instance
pub struct ClientModuleInstance {
    /// Instance id of the module
    pub id: ModuleInstanceId,
    /// Module-specific DB
    pub db: Database,
    /// Module-specific API
    pub api: DynModuleApi,
}

struct ClientInner {
    config: ClientConfig,
    decoders: ModuleDecoderRegistry,
    db: Database,
    federation_id: FederationId,
    federation_meta: BTreeMap<String, String>,
    primary_module_instance: ModuleInstanceId,
    modules: ClientModuleRegistry,
    executor: Executor<DynGlobalClientContext>,
    api: DynGlobalApi,
    root_secret: DerivableSecret,
    operation_log: OperationLog,
    secp_ctx: Secp256k1<secp256k1_zkp::All>,
}

impl ClientInner {
    fn primary_module(&self) -> &DynClientModule {
        self.modules
            .get(self.primary_module_instance)
            .expect("must have primary module")
    }

    fn context_gen(self: &Arc<Self>) -> ModuleGlobalContextGen {
        let client_inner = self.clone();
        Arc::new(move |module_instance, operation| {
            ModuleGlobalClientContext {
                client: client_inner.clone(),
                module_instance_id: module_instance,
                operation,
            }
            .into()
        })
    }

    fn config(&self) -> &ClientConfig {
        &self.config
    }

    fn decoders(&self) -> &ModuleDecoderRegistry {
        &self.decoders
    }

    /// Returns a reference to the module, panics if not found
    fn get_module(&self, instance: ModuleInstanceId) -> &maybe_add_send_sync!(dyn IClientModule) {
        self.try_get_module(instance)
            .expect("Module instance not found")
    }

    fn try_get_module(
        &self,
        instance: ModuleInstanceId,
    ) -> Option<&maybe_add_send_sync!(dyn IClientModule)> {
        Some(self.modules.get(instance)?.as_ref())
    }

    /// Determines if a transaction is underfunded, overfunded or balanced
    fn transaction_builder_balance(
        &self,
        builder: &TransactionBuilder,
    ) -> TransactionBuilderBalance {
        // FIXME: prevent overflows, currently not suitable for untrusted input
        let mut in_amount = Amount::ZERO;
        let mut out_amount = Amount::ZERO;
        let mut fee_amount = Amount::ZERO;

        for input in &builder.inputs {
            let module = self.get_module(input.input.module_instance_id());
            let item_amount = module.input_amount(&input.input);
            in_amount += item_amount.amount;
            fee_amount += item_amount.fee;
        }

        for output in &builder.outputs {
            let module = self.get_module(output.output.module_instance_id());
            let item_amount = module.output_amount(&output.output);
            out_amount += item_amount.amount;
            fee_amount += item_amount.fee;
        }

        let total_out_amount = out_amount + fee_amount;

        match total_out_amount.cmp(&in_amount) {
            Ordering::Equal => TransactionBuilderBalance::Balanced,
            Ordering::Less => TransactionBuilderBalance::Overfunded(in_amount - total_out_amount),
            Ordering::Greater => {
                TransactionBuilderBalance::Underfunded(total_out_amount - in_amount)
            }
        }
    }

    /// Adds funding to a transaction or removes overfunding via change.
    async fn finalize_transaction(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        mut partial_transaction: TransactionBuilder,
    ) -> anyhow::Result<(
        Transaction,
        Vec<DynState<DynGlobalClientContext>>,
        Option<u64>,
    )> {
        if let TransactionBuilderBalance::Underfunded(missing_amount) =
            self.transaction_builder_balance(&partial_transaction)
        {
            let input = self
                .primary_module()
                .create_sufficient_input(
                    self.primary_module_instance,
                    dbtx,
                    operation_id,
                    missing_amount,
                )
                .await?;
            partial_transaction.inputs.push(input);
        }

        let mut change_idx: Option<u64> = None;
        if let TransactionBuilderBalance::Overfunded(excess_amount) =
            self.transaction_builder_balance(&partial_transaction)
        {
            let output = self
                .primary_module()
                .create_exact_output(
                    self.primary_module_instance,
                    dbtx,
                    operation_id,
                    excess_amount,
                )
                .await;
            change_idx = Some(partial_transaction.outputs.len() as u64);
            partial_transaction.outputs.push(output);
        }

        assert!(
            matches!(
                self.transaction_builder_balance(&partial_transaction),
                TransactionBuilderBalance::Balanced
            ),
            "Transaction is balanced after the previous two operations"
        );

        let (tx, states) = partial_transaction.build(&self.secp_ctx, thread_rng());

        Ok((tx, states, change_idx))
    }

    async fn finalize_and_submit_transaction(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<(TransactionId, Option<OutPoint>)> {
        let (transaction, mut states, change_idx) = self
            .finalize_transaction(dbtx, operation_id, tx_builder)
            .await?;
        let txid = transaction.tx_hash();
        let change_outpoint = change_idx.map(|out_idx| OutPoint { txid, out_idx });

        let tx_submission_sm = DynState::from_typed(
            TRANSACTION_SUBMISSION_MODULE_INSTANCE,
            OperationState {
                operation_id,
                state: TxSubmissionStates::Created {
                    txid,
                    tx: transaction,
                    next_submission: now(),
                },
            },
        );
        states.push(tx_submission_sm);

        self.executor.add_state_machines_dbtx(dbtx, states).await?;

        Ok((txid, change_outpoint))
    }

    async fn transaction_update_stream(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, OperationState<TxSubmissionStates>> {
        self.executor
            .notifier()
            .module_notifier::<OperationState<TxSubmissionStates>>(
                TRANSACTION_SUBMISSION_MODULE_INSTANCE,
            )
            .subscribe(operation_id)
            .await
    }

    async fn operation_exists(
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
    ) -> bool {
        let active_state_exists = dbtx
            .find_by_prefix(&ActiveOperationStateKeyPrefix::<DynGlobalClientContext> {
                operation_id,
                _pd: Default::default(),
            })
            .await
            .next()
            .await
            .is_some();

        let inactive_state_exists = dbtx
            .find_by_prefix(&InactiveOperationStateKeyPrefix::<DynGlobalClientContext> {
                operation_id,
                _pd: Default::default(),
            })
            .await
            .next()
            .await
            .is_some();

        active_state_exists || inactive_state_exists
    }

    pub async fn await_primary_module_output(
        &self,
        operation_id: OperationId,
        out_point: OutPoint,
    ) -> anyhow::Result<Amount> {
        self.primary_module()
            .await_primary_module_output(operation_id, out_point)
            .await
    }
}

/// See [`Client::transaction_updates`]
pub struct TransactionUpdates {
    update_stream: BoxStream<'static, OperationState<TxSubmissionStates>>,
}

impl TransactionUpdates {
    /// Waits for the transaction to be accepted or rejected as part of the
    /// operation to which the `TransactionUpdates` object is subscribed.
    pub async fn await_tx_accepted(
        self,
        await_txid: TransactionId,
    ) -> Result<(), TxSubmissionError> {
        self.update_stream
            .filter_map(|tx_update| {
                std::future::ready(match tx_update.state {
                    TxSubmissionStates::Accepted { txid } if txid == await_txid => Some(Ok(())),
                    TxSubmissionStates::Rejected { txid, error } if txid == await_txid => {
                        Some(Err(error))
                    }
                    _ => None,
                })
            })
            .next_or_pending()
            .await
    }
}

#[derive(Default)]
pub struct ClientBuilder {
    module_gens: ClientModuleGenRegistry,
    primary_module_instance: Option<ModuleInstanceId>,
    config: Option<ClientConfig>,
    db: Option<DatabaseSource>,
}

pub enum DatabaseSource {
    Fresh(Box<dyn IDatabase>),
    Reuse(Client),
}

impl ClientBuilder {
    /// Replace module generator registry entirely
    pub fn with_module_gens(&mut self, module_gens: ClientModuleGenRegistry) {
        self.module_gens = module_gens;
    }

    /// Make module generator available when reading the config
    pub fn with_module<M: ClientModuleGen>(&mut self, module_gen: M) {
        self.module_gens.attach(module_gen);
    }

    /// Uses this config to initialize modules
    ///
    /// ## Panics
    /// If there was a config added previously
    pub fn with_config(&mut self, config: ClientConfig) {
        let was_replaced = self.config.replace(config).is_some();
        assert!(
            !was_replaced,
            "Only one config can be given to the builder."
        )
    }

    /// Uses this module with the given instance id as the primary module. See
    /// [`ClientModule::supports_being_primary`] for more information.
    ///
    /// ## Panics
    /// If there was a primary module specified previously
    pub fn with_primary_module(&mut self, primary_module_instance: ModuleInstanceId) {
        let was_replaced = self
            .primary_module_instance
            .replace(primary_module_instance)
            .is_some();
        assert!(
            !was_replaced,
            "Only one primary module can be given to the builder."
        )
    }

    // TODO: impl config from file
    // TODO: impl config from federation

    /// Uses this database to store the client state
    pub fn with_database<D: IDatabase + 'static>(&mut self, db: D) {
        self.with_dyn_database(Box::new(db));
    }

    /// Uses this database to store the client state, allowing for flexibility
    /// on the caller side by accepting a type-erased trait object.
    pub fn with_dyn_database(&mut self, db: Box<dyn IDatabase>) {
        let was_replaced = self.db.replace(DatabaseSource::Fresh(db)).is_some();
        assert!(
            !was_replaced,
            "Only one database can be given to the builder."
        );
    }

    /// Re-uses the database of an old client. Useful for restarting the client
    /// on recovery without fully shutting down the DB and not being able to
    /// re-open it.
    ///
    /// ## Panics
    /// If the old and new client use different config since that might make the
    /// DB incompatible.
    pub fn with_old_client_database(&mut self, client: Client) {
        let was_replaced = self.db.replace(DatabaseSource::Reuse(client)).is_some();
        assert!(
            !was_replaced,
            "Only one database can be given to the builder."
        );
    }

    pub async fn build_restoring_from_backup<S>(
        self,
        tg: &mut TaskGroup,
        secret: ClientSecret<S>,
    ) -> anyhow::Result<(Client, Metadata)>
    where
        S: RootSecretStrategy,
    {
        let fake_notifications = Default::default();
        let mut dbtx = match self.db.as_ref().expect("No database provided") {
            DatabaseSource::Fresh(db) => DatabaseTransaction::new(
                db.begin_transaction().await,
                Default::default(),
                &fake_notifications,
            ),
            DatabaseSource::Reuse(db) => db.db().begin_transaction().await,
        };

        // TODO: assert DB is empty (what does that mean? maybe needs a method that
        // checks if "wipe" was called on modules?)

        // let db_is_empty = raw_dbtx
        //     .raw_find_by_prefix(&[])
        //     .await
        //     .expect("DB read failed")
        //     .next()
        //     .await
        //     .is_none();
        // assert!(
        //     db_is_empty,
        //     "Database is not empty, cannot restore from backup"
        // );

        // Write new root secret to DB before starting client
        set_client_root_secret(&mut dbtx, &secret).await;
        dbtx.commit_tx().await;

        let client = self.build::<S>(tg).await?;
        let metadata = client.restore_from_backup().await?;

        Ok((client, metadata))
    }

    /// Build a [`Client`] and start its executor
    pub async fn build<S>(self, tg: &mut TaskGroup) -> anyhow::Result<Client>
    where
        S: RootSecretStrategy,
    {
        let client = self.build_stopped::<S>().await?;
        client.start_executor(tg).await;
        Ok(client)
    }

    /// Build a [`Client`] but do not start the executor
    pub async fn build_stopped<S>(self) -> anyhow::Result<Client>
    where
        S: RootSecretStrategy,
    {
        let config = self.config.ok_or(anyhow!("No config was provided"))?;
        let primary_module_instance = self
            .primary_module_instance
            .ok_or(anyhow!("No primary module instance id was provided"))?;

        let mut decoders = client_decoders(
            &self.module_gens,
            config
                .modules
                .iter()
                .map(|(module_instance, module_config)| (*module_instance, module_config.kind())),
        )?;
        decoders.register_module(
            TRANSACTION_SUBMISSION_MODULE_INSTANCE,
            ModuleKind::from_static_str("tx_submission"),
            tx_submission_sm_decoder(),
        );

        let db = match self.db.ok_or(anyhow!("No database was provided"))? {
            DatabaseSource::Fresh(db) => Database::new_from_box(db, decoders.clone()),
            DatabaseSource::Reuse(client) => {
                assert_eq!(
                    client.inner.config, config,
                    "Can only reuse DB for clients started with the same config"
                );
                client.inner.db.clone()
            }
        };

        let notifier = Notifier::new(db.clone());

        let api = DynGlobalApi::from(WsFederationApi::from_config(&config));

        let root_secret = get_client_root_secret::<S>(&db).await;

        let modules = {
            let mut modules = ClientModuleRegistry::default();
            for (module_instance, module_config) in config.modules.clone() {
                let kind = module_config.kind().clone();
                let module_gen = match self.module_gens.get(&kind) {
                    Some(module_gen) => module_gen,
                    None => {
                        info!("Module kind {kind} of instance {module_instance} not found in module gens, skipping");
                        continue;
                    }
                };

                let module = module_gen
                    .init(
                        module_config,
                        db.clone(),
                        module_instance,
                        // This is a divergence from the legacy client, where the child secret
                        // keys were derived using *module kind*-specific derivation paths.
                        // Since the new client has to support multiple, segregated modules of
                        // the same kind we have to use the instance id instead.
                        root_secret.derive_module_secret(module_instance),
                        notifier.clone(),
                        api.clone(),
                    )
                    .await?;

                if primary_module_instance == module_instance && !module.supports_being_primary() {
                    bail!("Module instance {primary_module_instance} of kind {kind} does not support being a primary module");
                }

                modules.register_module(module_instance, kind, module);
            }
            modules
        };

        let executor = {
            let mut executor_builder = Executor::<DynGlobalClientContext>::builder();
            executor_builder
                .with_module(TRANSACTION_SUBMISSION_MODULE_INSTANCE, TxSubmissionContext);

            for (module_instance_id, _, module) in modules.iter_modules() {
                executor_builder.with_module_dyn(module.context(module_instance_id));
            }

            executor_builder.build(db.clone(), notifier).await
        };

        let client_inner = Arc::new(ClientInner {
            config: config.clone(),
            decoders,
            db: db.clone(),
            federation_id: config.federation_id,
            federation_meta: config.meta,
            primary_module_instance,
            modules,
            executor,
            api,
            secp_ctx: Secp256k1::new(),
            root_secret,
            operation_log: OperationLog::new(db),
        });

        Ok(Client {
            inner: client_inner,
        })
    }
}

/// Fetches the client secret encoding from the database or generates a new one
/// if none is present
pub async fn get_client_root_secret_encoding<S>(db: &Database) -> S::Encoding
where
    S: RootSecretStrategy,
{
    let mut tx = db.begin_transaction().await;
    let client_secret = tx.get_value(&ClientSecretKey::<S>::default()).await;
    let secret = if let Some(client_secret) = client_secret {
        client_secret.0
    } else {
        let secret = S::random(&mut thread_rng());
        let no_replacement = tx
            .insert_entry(
                &ClientSecretKey::<S>::default(),
                &ClientSecret(secret.clone()),
            )
            .await
            .is_none();
        assert!(
            no_replacement,
            "We would have overwritten our secret key, aborting!"
        );
        secret
    };
    tx.commit_tx().await;
    secret
}

/// Sets the client secret in the database, returns if an old secret was
/// overwritten
async fn set_client_root_secret<S>(
    dbtx: &mut DatabaseTransaction<'_>,
    secret: &ClientSecret<S>,
) -> bool
where
    S: RootSecretStrategy,
{
    dbtx.insert_entry(&ClientSecretKey::<S>::default(), secret)
        .await
        .is_some()
}

/// Fetches the client secret from the database or generates a new one if
/// none is present
pub async fn get_client_root_secret<S>(db: &Database) -> DerivableSecret
where
    S: RootSecretStrategy,
{
    let encoding = get_client_root_secret_encoding::<S>(db).await;
    S::to_root_secret(&encoding)
}

/// Secret input key material from which the [`DerivableSecret`] used by the
/// client will be seeded
pub struct ClientSecret<S: RootSecretStrategy>(S::Encoding);

impl<S> ClientSecret<S>
where
    S: RootSecretStrategy,
{
    pub fn new(key: S::Encoding) -> Self {
        Self(key)
    }
}

impl<ES> Serialize for ClientSecret<ES>
where
    ES: RootSecretStrategy,
{
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut bytes = Vec::new();
        ES::consensus_encode(&self.0, &mut bytes).expect("Writing to vec can't fail");
        serializer.serialize_bytes(&bytes)
    }
}

impl<S> Encodable for ClientSecret<S>
where
    S: RootSecretStrategy,
{
    fn consensus_encode<W: Write>(&self, writer: &mut W) -> std::result::Result<usize, Error> {
        S::consensus_encode(&self.0, writer)
    }
}

impl<S> Decodable for ClientSecret<S>
where
    S: RootSecretStrategy,
{
    fn consensus_decode<R: Read>(
        reader: &mut R,
        _modules: &ModuleDecoderRegistry,
    ) -> std::result::Result<Self, DecodeError> {
        Ok(ClientSecret(S::consensus_decode(reader)?))
    }
}

impl<S> Debug for ClientSecret<S>
where
    S: RootSecretStrategy,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClientSecret([redacted])")
    }
}

pub fn client_decoders<'a>(
    registry: &ModuleGenRegistry<DynClientModuleGen>,
    module_kinds: impl Iterator<Item = (ModuleInstanceId, &'a ModuleKind)>,
) -> anyhow::Result<ModuleDecoderRegistry> {
    let mut modules = BTreeMap::new();
    for (id, kind) in module_kinds {
        let Some(init) = registry.get(kind) else {
            info!("Detected configuration for unsupported module kind: {kind}");
            continue
        };

        modules.insert(
            id,
            (
                kind.clone(),
                IClientModuleGen::decoder(AsRef::<dyn IClientModuleGen + 'static>::as_ref(init)),
            ),
        );
    }
    Ok(ModuleDecoderRegistry::from(modules))
}
