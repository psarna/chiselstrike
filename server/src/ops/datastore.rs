// SPDX-FileCopyrightText: © 2022 ChiselStrike <info@chiselstrike.com>

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use deno_core::error::AnyError;
use deno_core::{serde_v8, v8, CancelFuture};
use serde_derive::Deserialize;

use super::WorkerState;
use crate::datastore::engine::{IdTree, QueryResults, TransactionStatic};
use crate::datastore::expr::Expr;
use crate::datastore::query::{Mutation, QueryOpChain, QueryPlan, RequestContext};
use crate::datastore::value::EntityValue;
use crate::datastore::{crud, QueryEngine};
use crate::policies::PolicySystem;
use crate::server::Server;
use crate::types::{Type, TypeSystem};
use crate::version::Version;
use crate::JsonObject;

/// ChiselRequestContext corresponds to `requestContext` structure used in chisel.ts.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChiselRequestContext {
    version_id: String,
    //method: String,
    headers: Vec<(String, String)>,
    path: String,
    routing_path: String,
    user_id: Option<String>,
}

#[deno_core::op]
pub async fn op_chisel_begin_transaction(state: Rc<RefCell<deno_core::OpState>>) -> Result<()> {
    let query_engine = state
        .borrow()
        .borrow::<WorkerState>()
        .server
        .query_engine
        .clone();
    let transaction = query_engine.begin_transaction_static().await?;
    {
        let mut state = state.borrow_mut();
        let worker_state = state.borrow_mut::<WorkerState>();
        ensure!(
            worker_state.transaction.is_none(),
            "Cannot begin a transaction because another transaction is in progress"
        );
        worker_state.transaction = Some(transaction);
    }
    Ok(())
}

#[deno_core::op]
pub async fn op_chisel_commit_transaction(state: Rc<RefCell<deno_core::OpState>>) -> Result<()> {
    let transaction = state
        .borrow_mut()
        .borrow_mut::<WorkerState>()
        .transaction
        .take()
        .context("Cannot commit a transaction because no transaction is in progress")?;
    let transaction = Arc::try_unwrap(transaction)
        .ok()
        .context(
            "Cannot commit a transaction because there is an operation \
            in progress that uses this transaction",
        )?
        .into_inner();
    QueryEngine::commit_transaction(transaction).await?;
    Ok(())
}

#[deno_core::op]
pub fn op_chisel_rollback_transaction(state: &mut deno_core::OpState) -> Result<()> {
    let transaction = state
        .borrow_mut::<WorkerState>()
        .transaction
        .take()
        .context("Cannot rollback a transaction because no transaction is in progress")?;
    let transaction = Arc::try_unwrap(transaction)
        .ok()
        .context(
            "Cannot rollback a transaction because there is an operation \
            in progress that uses this transaction",
        )?
        .into_inner();
    // Drop the transaction, causing it to rollback.
    drop(transaction);
    Ok(())
}

async fn with_transaction<F, Fut, T>(state: Rc<RefCell<deno_core::OpState>>, f: F) -> Result<T>
where
    F: FnOnce(Arc<Server>, Arc<Version>, TransactionStatic) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let fut = {
        let state = state.borrow();
        let worker_state = state.borrow::<WorkerState>();
        let server = worker_state.server.clone();
        let version = worker_state.version.clone();
        let transaction = worker_state
            .transaction
            .clone()
            .context("Cannot perform a data operation because no transaction is in progress")?;
        f(server, version, transaction)
    };
    fut.await
}

#[derive(Deserialize)]
pub struct StoreParams<'a> {
    name: String,
    value: serde_v8::Value<'a>,
}

#[deno_core::op(v8)]
pub fn op_chisel_store<'a>(
    scope: &mut v8::HandleScope<'a>,
    state: Rc<RefCell<deno_core::OpState>>,
    params: StoreParams<'a>,
    context: ChiselRequestContext,
) -> Result<impl Future<Output = Result<IdTree, AnyError>> + 'static, AnyError> {
    let v8_value = &params.value.v8_value;
    let value = EntityValue::from_v8(v8_value, scope)?;

    Ok(async move {
        with_transaction(state, move |server, version, transaction| async move {
            let ty = match version.type_system.lookup_type(&params.name) {
                Ok(Type::Entity(ty)) => ty,
                _ => bail!("Cannot save into type {}", params.name),
            };
            if ty.is_auth() && !is_auth_path(&context.version_id, &context.routing_path) {
                bail!("Cannot save into auth type {}", params.name);
            }

            let mut transaction = transaction.lock().await;
            server
                .query_engine
                .add_row(
                    &ty,
                    value.as_map()?,
                    Some(&mut transaction),
                    &version.type_system,
                )
                .await
        })
        .await
    })
}

fn is_auth_path(version_id: &str, routing_path: &str) -> bool {
    version_id == "__chiselstrike" && routing_path.starts_with("/auth/")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    type_name: String,
    filter_expr: Option<Expr>,
}

#[deno_core::op]
pub async fn op_chisel_delete(
    state: Rc<RefCell<deno_core::OpState>>,
    params: DeleteParams,
    context: ChiselRequestContext,
) -> Result<()> {
    with_transaction(state, move |server, version, transaction| async move {
        let mutation = Mutation::delete_from_expr(
            &RequestContext::new(&version.policy_system, &version.type_system, context),
            &params.type_name,
            &params.filter_expr,
        )
        .context("failed to construct delete expression from JSON passed to `op_chisel_delete`")?;

        let mut transaction = transaction.lock().await;
        server
            .query_engine
            .mutate_with_transaction(mutation, &mut transaction)
            .await?;
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrudDeleteParams {
    type_name: String,
    url_query: Vec<(String, String)>,
}

#[deno_core::op]
pub async fn op_chisel_crud_delete(
    state: Rc<RefCell<deno_core::OpState>>,
    params: CrudDeleteParams,
    context: ChiselRequestContext,
) -> Result<()> {
    with_transaction(state, move |server, version, transaction| async move {
        let mutation = crud::delete_from_url_query(
            &RequestContext::new(&version.policy_system, &version.type_system, context),
            &params.type_name,
            &params.url_query,
        )
        .context(
            "failed to construct delete expression from JSON passed to `op_chisel_crud_delete`",
        )?;

        let mut transaction = transaction.lock().await;
        server
            .query_engine
            .mutate_with_transaction(mutation, &mut transaction)
            .await?;
        Ok(())
    })
    .await
}

#[deno_core::op]
pub async fn op_chisel_crud_query(
    state: Rc<RefCell<deno_core::OpState>>,
    params: crud::QueryParams,
    context: ChiselRequestContext,
) -> Result<JsonObject> {
    with_transaction(state, move |server, version, transaction| async move {
        crud::run_query(
            &RequestContext::new(&version.policy_system, &version.type_system, context),
            params,
            server.query_engine.clone(),
            transaction,
        )
        .await
    })
    .await
}

#[deno_core::op]
pub async fn op_chisel_relational_query_create(
    state: Rc<RefCell<deno_core::OpState>>,
    op_chain: QueryOpChain,
    context: ChiselRequestContext,
) -> Result<deno_core::ResourceId> {
    with_transaction(
        state.clone(),
        move |server, version, transaction| async move {
            let query_plan = QueryPlan::from_op_chain(
                &RequestContext::new(&version.policy_system, &version.type_system, context),
                op_chain,
            )?;

            let stream = server.query_engine.query(transaction, query_plan)?;
            let resource = QueryStreamResource {
                stream: RefCell::new(stream),
                cancel: Default::default(),
                next: RefCell::new(None),
            };
            let rid = state.borrow_mut().resource_table.add(resource);
            Ok(rid)
        },
    )
    .await
}

type DbStream = RefCell<QueryResults>;

struct QueryStreamResource {
    stream: DbStream,
    cancel: deno_core::CancelHandle,
    next: RefCell<Option<EntityValue>>,
}

impl deno_core::Resource for QueryStreamResource {
    fn close(self: Rc<Self>) {
        self.cancel.cancel();
    }
}

// A future that resolves when this stream next element is available.
struct QueryNextFuture {
    resource: Weak<QueryStreamResource>,
}

impl Future for QueryNextFuture {
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.resource.upgrade() {
            Some(rc) => {
                let mut stream = rc.stream.borrow_mut();
                let stream: &mut QueryResults = &mut stream;
                match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(next))) => {
                        *rc.next.borrow_mut() = Some(EntityValue::Map(next));
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Some(Err(e))) => Poll::Ready(Err(e)),
                    Poll::Ready(None) => Poll::Ready(Ok(())),
                    Poll::Pending => Poll::Pending,
                }
            }
            None => Poll::Ready(Err(anyhow!("Closed resource"))),
        }
    }
}

#[deno_core::op]
pub async fn op_chisel_query_next(
    state: Rc<RefCell<deno_core::OpState>>,
    query_stream_rid: deno_core::ResourceId,
) -> Result<()> {
    let (resource, cancel) = {
        let rc: Rc<QueryStreamResource> = state.borrow().resource_table.get(query_stream_rid)?;
        let cancel = deno_core::RcRef::map(&rc, |r| &r.cancel);
        (Rc::downgrade(&rc), cancel)
    };

    let fut = QueryNextFuture { resource };
    let fut = fut.or_cancel(cancel);
    fut.await?
}

#[deno_core::op(v8)]
pub fn op_chisel_query_get_value<'a>(
    scope: &mut v8::HandleScope<'a>,
    state: Rc<RefCell<deno_core::OpState>>,
    query_stream_rid: deno_core::ResourceId,
) -> Result<serde_v8::Value<'a>> {
    let query_stream: Rc<QueryStreamResource> =
        state.borrow().resource_table.get(query_stream_rid)?;
    let v8_value = match query_stream.next.borrow_mut().take() {
        Some(v) => v.to_v8(scope)?,
        None => v8::null(scope).into(),
    };
    Ok(serde_v8::Value::from(v8_value))
}

impl RequestContext<'_> {
    fn new<'a>(
        ps: &'a PolicySystem,
        ts: &'a TypeSystem,
        context: ChiselRequestContext,
    ) -> RequestContext<'a> {
        RequestContext {
            ps,
            ts,
            version_id: context.version_id,
            user_id: context.user_id,
            path: context.path,
            headers: context.headers.into_iter().collect(),
        }
    }
}
