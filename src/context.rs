use core::cell::{Cell, RefCell};
use core::pin::Pin;
use core::time::Duration;

use futures_util::future::{select, Either};
use futures_util::stream::{FuturesUnordered, Stream, StreamExt};
use slab::Slab;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;

use crate::actor::{Actor, ActorState, CHANNEL_CAP};
use crate::address::{Addr, WeakAddr};
use crate::handler::Handler;
use crate::message::{
    ActorMessage, FunctionMessage, FunctionMutMessage, IntervalMessage, Message, MessageObject,
};
use crate::types::LocalBoxedFuture;

pub struct Context<A> {
    state: Cell<ActorState>,
    interval_queue: RefCell<Slab<IntervalMessage<A>>>,
    delay_queue: RefCell<Slab<ActorMessage<A>>>,
    tx: WeakAddr<A>,
    rx: RefCell<Receiver<ActorMessage<A>>>,
}

/// a join handle can be used to cancel a spawned async task like interval closure and stream
/// handler
pub struct ContextJoinHandle {
    handle: oneshot::Sender<()>,
}

impl ContextJoinHandle {
    pub fn cancel(self) {
        let _ = self.handle.send(());
    }
}

impl<A: Actor> Context<A> {
    pub(crate) fn new(tx: WeakAddr<A>, rx: Receiver<ActorMessage<A>>) -> Self {
        Context {
            state: Cell::new(ActorState::Stop),
            interval_queue: RefCell::new(Slab::with_capacity(8)),
            delay_queue: RefCell::new(Slab::with_capacity(CHANNEL_CAP)),
            tx,
            rx: RefCell::new(rx),
        }
    }

    /// run interval concurrent closure on context. `Handler::handle` will be called.
    pub fn run_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, ()> + Clone + 'static,
    {
        let msg = FunctionMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Ref(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run interval exclusive closure on context. `Handler::handle_wait` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_interval<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, ()>
            + Clone
            + 'static,
    {
        let msg = FunctionMutMessage::<F, ()>::new(f);
        let msg = IntervalMessage::Mut(Box::new(msg));
        self.interval(dur, msg)
    }

    /// run concurrent closure on context after given duration. `Handler::handle` will be called.
    pub fn run_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a A, &'a Context<A>) -> LocalBoxedFuture<'a, ()> + 'static,
    {
        let msg = FunctionMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Ref(msg))
    }

    /// run exclusive closure on context after given duration. `Handler::handle_wait` will be
    /// called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    pub fn run_wait_later<F>(&self, dur: Duration, f: F) -> ContextJoinHandle
    where
        F: for<'a> FnOnce(&'a mut A, &'a mut Context<A>) -> LocalBoxedFuture<'a, ()> + 'static,
    {
        let msg = FunctionMutMessage::new(f);
        let msg = MessageObject::new(msg, None);
        self.later(dur, ActorMessage::Mut(msg))
    }

    /// stop the context. It would end the actor gracefully by draining all remaining message in
    /// queue.
    ///
    /// *. It DOES NOT drain the channel.
    pub fn stop(&self) {
        self.rx.borrow_mut().close();
        self.state.set(ActorState::StopGraceful);
    }

    /// get the address of actor from context.
    pub fn address(&self) -> Option<Addr<A>> {
        self.tx.upgrade()
    }

    /// add a stream to context. multiple stream can be added to one context.
    ///
    /// stream item will be treated as concurrent message and `Handler::handle` will be called.
    /// If `Handler::handle_wait` is not override `Handler::handle` will be called as fallback.
    /// # example:
    /// ```rust
    /// use actix_async::prelude::*;
    /// use futures_util::stream::once;
    ///
    /// struct StreamActor;
    ///
    /// impl Actor for StreamActor {
    ///     type Runtime = ActixRuntime;
    /// }
    ///
    /// struct StreamMessage;
    ///
    /// impl Message for StreamMessage {
    ///     type Result = ();
    /// }
    ///
    /// #[async_trait::async_trait(?Send)]
    /// impl Handler<StreamMessage> for StreamActor {
    ///     async fn handle(&self, _: StreamMessage, _: &Context<Self>) {}
    /// }
    ///
    /// #[actix_rt::main]
    /// async fn main() {
    ///     let address = StreamActor::create(|ctx| {
    ///         ctx.add_stream(once(async { StreamMessage }));
    ///         StreamActor
    ///     });
    /// }
    /// ```
    pub fn add_stream<S, M>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
    {
        self.stream(stream, ActorMessage::Ref)
    }

    /// add a stream to context. multiple stream can be added to one context.
    ///
    /// stream item will be treated as exclusve message and `Handler::handle_wait` will be called.
    pub fn add_wait_stream<S, M>(&self, stream: S) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
    {
        self.stream(stream, ActorMessage::Mut)
    }

    fn stream<S, M, F>(&self, stream: S, f: F) -> ContextJoinHandle
    where
        S: Stream<Item = M> + 'static,
        M: Message + 'static,
        A: Handler<M>,
        F: FnOnce(MessageObject<A>) -> ActorMessage<A> + Copy + 'static,
    {
        let weak_tx = self.tx.clone();
        let (tx_cancel, mut rx_cancel) = oneshot::channel();

        A::spawn(async move {
            let mut stream = stream;
            // SAFETY:
            // stream is owned by async task and never moved. The loop would borrow pinned stream
            // with `Next`.
            let mut stream = unsafe { Pin::new_unchecked(&mut stream) };
            loop {
                match select(&mut rx_cancel, stream.next()).await {
                    // join handle notify to cancel.
                    Either::Left((Ok(_), _)) => return,
                    // join handle is dropped so don't listen to it anymore.
                    Either::Left((Err(_), s)) => match s.await {
                        Some(msg) => {
                            let msg = MessageObject::new(msg, None);
                            match weak_tx._send(f(msg)).await {
                                Ok(()) => break,
                                Err(_) => return,
                            }
                        }
                        None => return,
                    },
                    Either::Right((Some(msg), _)) => {
                        let msg = MessageObject::new(msg, None);
                        if weak_tx._send(f(msg)).await.is_err() {
                            return;
                        }
                    }
                    Either::Right((None, _)) => return,
                }
            }

            // join handle is gone and iter with stream only.
            while let Some(msg) = stream.next().await {
                let msg = MessageObject::new(msg, None);
                if weak_tx._send(f(msg)).await.is_err() {
                    return;
                }
            }
        });

        ContextJoinHandle { handle: tx_cancel }
    }

    fn interval(&self, dur: Duration, msg: IntervalMessage<A>) -> ContextJoinHandle {
        let token = self.interval_queue.borrow_mut().insert(msg);

        let weak_tx = self.tx.clone();
        let (tx_cancel, mut rx_cancel) = oneshot::channel();

        A::spawn(async move {
            let mut sleep = A::sleep(dur);
            loop {
                match select(&mut rx_cancel, &mut sleep).await {
                    // join handle notify to cancel.
                    Either::Left((Ok(_), _)) => {
                        let _ = weak_tx
                            ._send(ActorMessage::IntervalTokenCancel(token))
                            .await;
                        return;
                    }
                    // join handle is dropped so don't listen to it anymore.
                    Either::Left((Err(_), s)) => {
                        s.await;
                        break;
                    }
                    Either::Right(_) => {
                        match weak_tx._send(ActorMessage::IntervalToken(token)).await {
                            Ok(()) => {
                                sleep = A::sleep(dur);
                                continue;
                            }
                            Err(_) => return,
                        }
                    }
                }
            }

            // join handle is gone and iter with sleep only.
            loop {
                match weak_tx._send(ActorMessage::IntervalToken(token)).await {
                    Ok(()) => A::sleep(dur).await,
                    Err(_) => return,
                }
            }
        });

        ContextJoinHandle { handle: tx_cancel }
    }

    fn later(&self, dur: Duration, msg: ActorMessage<A>) -> ContextJoinHandle {
        let token = self.delay_queue.borrow_mut().insert(msg);
        let weak_tx = self.tx.clone();
        let (tx_cancel, rx_cancel) = oneshot::channel();

        A::spawn(async move {
            match select(rx_cancel, A::sleep(dur)).await {
                Either::Left((Ok(_), _)) => {
                    let _ = weak_tx._send(ActorMessage::DelayTokenCancel(token)).await;
                    return;
                }
                Either::Left((Err(_), s)) => s.await,
                Either::Right(_) => (),
            }
            let _ = weak_tx._send(ActorMessage::DelayToken(token)).await;
        });

        ContextJoinHandle { handle: tx_cancel }
    }

    async fn try_handle_concurrent(
        &self,
        actor: &A,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut FuturesUnordered<LocalBoxedFuture<'static, ()>>,
    ) {
        if !cache_ref.is_empty() {
            cache_ref.iter_mut().for_each(|m| {
                // SAFETY:
                // `FuturesUnordered` can not tied to actor and context's lifetime here.
                // It has no idea if the futures are all resolved in this scope and would assume
                // the boxed futures would live as long as the actor and context.
                // Making it impossible to mutably borrow them from this point on.
                //
                // All futures transmuted to static lifetime must resolved before exiting
                // try_handle_concurrent method.
                let m: LocalBoxedFuture<'static, ()> =
                    unsafe { std::mem::transmute(m.handle(actor, self)) };

                fut.push(m);
            });

            // drain the unordered future before going on.
            while fut.next().await.is_some() {}

            // clear the cache as they are all finished.
            cache_ref.clear();
        }

        debug_assert!(fut.next().await.is_none());
    }

    async fn handle_exclusive(
        &mut self,
        msg: MessageObject<A>,
        actor: &mut A,
        cache_mut: &mut Option<MessageObject<A>>,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut FuturesUnordered<LocalBoxedFuture<'static, ()>>,
    ) {
        // put message in cache in case thread panic before it's handled
        *cache_mut = Some(msg);
        // try handle concurrent messages first.
        self.try_handle_concurrent(&*actor, cache_ref, fut).await;
        // pop the cache and handle
        cache_mut.take().unwrap().handle_wait(actor, self).await;
    }

    fn handle_delay_cancel(&mut self, token: usize) {
        if self.delay_queue.borrow().contains(token) {
            self.delay_queue.borrow_mut().remove(token);
        }
    }

    fn handle_interval_cancel(&self, token: usize) {
        if self.interval_queue.borrow().contains(token) {
            self.interval_queue.borrow_mut().remove(token);
        }
    }

    // handle single message and return true if context is force stopping.
    async fn handle_message(
        &mut self,
        msg: ActorMessage<A>,
        actor: &mut A,
        cache_mut: &mut Option<MessageObject<A>>,
        cache_ref: &mut Vec<MessageObject<A>>,
        fut: &mut FuturesUnordered<LocalBoxedFuture<'static, ()>>,
        drop_notify: &mut Option<oneshot::Sender<()>>,
    ) -> bool {
        match msg {
            // have exclusive messages.
            ActorMessage::Mut(msg) => {
                self.handle_exclusive(msg, actor, cache_mut, cache_ref, fut)
                    .await
            }
            // have concurrent message.
            ActorMessage::Ref(msg) => cache_ref.push(msg),
            ActorMessage::DelayToken(token) => {
                if self.delay_queue.borrow().contains(token) {
                    let msg = self.delay_queue.borrow_mut().remove(token);
                    match msg {
                        ActorMessage::Ref(msg) => cache_ref.push(msg),
                        ActorMessage::Mut(msg) => {
                            self.handle_exclusive(msg, actor, cache_mut, cache_ref, fut)
                                .await
                        }
                        _ => unreachable!(
                            "Delay message can only be ActorMessage::Ref or ActorMessage::Mut"
                        ),
                    }
                }
            }
            ActorMessage::IntervalToken(token) => {
                let msg = match self.interval_queue.borrow().get(token) {
                    Some(msg) => msg.clone_actor_message(),
                    None => return false,
                };
                match msg {
                    ActorMessage::Mut(msg) => {
                        self.handle_exclusive(msg, actor, cache_mut, cache_ref, fut)
                            .await
                    }
                    ActorMessage::Ref(msg) => cache_ref.push(msg),
                    _ => {
                        unreachable!(
                            "Only ActorMessage::Ref and ActorMessage::Mut can use delay queue"
                        )
                    }
                }
            }
            ActorMessage::DelayTokenCancel(token) => self.handle_delay_cancel(token),
            ActorMessage::IntervalTokenCancel(token) => self.handle_interval_cancel(token),
            ActorMessage::ActorState(state, notify) => {
                *drop_notify = notify;
                if state != ActorState::Running {
                    self.stop();
                };

                return state == ActorState::Stop;
            }
        };

        false
    }
}

pub(crate) struct ContextWithActor<A: Actor> {
    ctx: Option<Context<A>>,
    actor: Option<A>,
    cache_mut: Option<MessageObject<A>>,
    cache_ref: Vec<MessageObject<A>>,
    drop_notify: Option<oneshot::Sender<()>>,
}

impl<A: Actor> Default for ContextWithActor<A> {
    fn default() -> Self {
        Self {
            ctx: None,
            actor: None,
            cache_mut: None,
            cache_ref: Vec::new(),
            drop_notify: None,
        }
    }
}

impl<A: Actor> Drop for ContextWithActor<A> {
    fn drop(&mut self) {
        // recovery from thread panic.
        if std::thread::panicking() && self.ctx.as_ref().unwrap().state.get() == ActorState::Running
        {
            let mut ctx = std::mem::take(self);
            // some of the cached message object may finished gone. remove them.
            ctx.cache_ref.retain(|m| !m.finished());

            A::spawn(async move {
                let _ = ctx.run().await;
            });
        } else if let Some(tx) = self.drop_notify.take() {
            let _ = tx.send(());
        }
    }
}

impl<A: Actor> ContextWithActor<A> {
    pub(crate) fn new(actor: A, ctx: Context<A>) -> Self {
        Self {
            actor: Some(actor),
            ctx: Some(ctx),
            cache_mut: None,
            cache_ref: Vec::with_capacity(CHANNEL_CAP),
            drop_notify: None,
        }
    }

    pub(crate) async fn first_run(&mut self) {
        let actor = self.actor.as_mut().unwrap();
        let ctx = self.ctx.as_mut().unwrap();

        actor.on_start(ctx).await;
        ctx.state.set(ActorState::Running);

        self.run().await;
    }

    async fn run(&mut self) {
        let actor = self.actor.as_mut().unwrap();
        let ctx = self.ctx.as_mut().unwrap();
        let cache_mut = &mut self.cache_mut;
        let cache_ref = &mut self.cache_ref;
        let drop_notify = &mut self.drop_notify;

        let mut fut = FuturesUnordered::new();

        // if there is cached message it must be dealt with
        ctx.try_handle_concurrent(&*actor, cache_ref, &mut fut)
            .await;

        if let Some(mut msg) = cache_mut.take() {
            msg.handle_wait(actor, ctx).await;
        }

        // batch receive new messages from channel.
        loop {
            match ctx.rx.get_mut().try_recv() {
                Ok(msg) => {
                    let is_force_stop = ctx
                        .handle_message(msg, actor, cache_mut, cache_ref, &mut fut, drop_notify)
                        .await;

                    if is_force_stop {
                        break;
                    }
                }

                Err(TryRecvError::Empty) => {
                    // channel is empty. try to handle concurrent messages from previous iters.
                    ctx.try_handle_concurrent(actor, cache_ref, &mut fut).await;

                    // block the task and recv one message when channel is empty.
                    match ctx.rx.get_mut().recv().await {
                        Some(msg) => {
                            let is_force_stop = ctx
                                .handle_message(
                                    msg,
                                    actor,
                                    cache_mut,
                                    cache_ref,
                                    &mut fut,
                                    drop_notify,
                                )
                                .await;

                            if is_force_stop {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                Err(TryRecvError::Closed) => {
                    // channel is closed. stop the context.
                    ctx.stop();
                    // try to handle concurrent messages from previous iters.
                    ctx.try_handle_concurrent(&*actor, cache_ref, &mut fut)
                        .await;

                    break;
                }
            };
        }

        actor.on_stop(ctx).await;
    }
}
