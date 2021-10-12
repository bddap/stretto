cfg_async! {
    use crate::metrics::{MetricType, Metrics};
    use crate::policy::LFUPolicy;
    use crate::store::ShardedMap;
    use crate::ttl::{ExpirationMap, Time};
    use crate::{
        CacheCallback, CacheError, Coster, DefaultCacheCallback, DefaultCoster, DefaultUpdateValidator,
        KeyBuilder, UpdateValidator,
    };
    use crate::cache::{CacheBuilder, Item, WaitGroup, CacheCleaner};
    use std::hash::{Hash, BuildHasher};
    use std::collections::hash_map::RandomState;
    use std::sync::Arc;
    use tokio::sync::mpsc::{Sender, Receiver, channel, UnboundedReceiver, unbounded_channel, UnboundedSender};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::{Duration, sleep, Instant};
    use std::marker::PhantomData;
    use tokio::task::JoinHandle;
    use std::collections::HashMap;


    impl<K, V, KH, C, U, CB, PS, ES, SS> CacheBuilder<K, V, KH, C, U, CB, PS, ES, SS>
        where
            K: Hash + Eq,
            V: Send + Sync + 'static,
            KH: KeyBuilder<K>,
            C: Coster<V>,
            U: UpdateValidator<V>,
            CB: CacheCallback<V>,
            PS: BuildHasher + Clone + 'static,
            ES: BuildHasher + Clone + 'static,
            SS: BuildHasher + Clone + 'static,
    {
        pub fn finalize(self) -> Result<Cache<K, V, KH, C, U, CB, PS, ES, SS>, CacheError> {
            let num_counters = self.num_counters;

            if num_counters == 0 {
                return Err(CacheError::InvalidNumCounters);
            }

            let max_cost = self.max_cost;
            if max_cost == 0 {
                return Err(CacheError::InvalidMaxCost);
            }

            let insert_buffer_size = self.insert_buffer_size;
            if insert_buffer_size == 0 {
                return Err(CacheError::InvalidBufferSize);
            }

            let (buf_tx, buf_rx) = channel(insert_buffer_size);
            let (stop_tx, stop_rx) = channel(1);
            let (clear_tx, clear_rx) = unbounded_channel();

            let expiration_map = ExpirationMap::with_hasher(self.expiration_hasher.unwrap());

            let store = Arc::new(ShardedMap::with_validator_and_hasher(
                expiration_map,
                self.update_validator.unwrap(),
                self.store_hasher.unwrap(),
            ));

            let mut policy =
                LFUPolicy::with_hasher(num_counters, max_cost, self.policy_hasher.unwrap())?;

            let item_size = store.item_size();

            let coster = Arc::new(self.coster.unwrap());
            let callback = Arc::new(self.callback.unwrap());
            let metrics = if self.metrics {
                let m = Arc::new(Metrics::new_op());
                policy.collect_metrics(m.clone());
                m
            } else {
                Arc::new(Metrics::new())
            };

            let policy = Arc::new(policy);
            CacheProcessor::spawn(
                100000,
                self.ignore_internal_cost,
                self.cleanup_duration,
                store.clone(),
                policy.clone(),
                buf_rx,
                stop_rx,
                clear_rx,
                metrics.clone(),
                coster.clone(),
                callback.clone(),
            );

            let this = Cache {
                store,
                policy,
                insert_buf_tx: buf_tx,
                callback,
                key_to_hash: self.key_to_hash,
                stop_tx,
                clear_tx,
                is_closed: AtomicBool::new(false),
                coster,
                ignore_internal_cost: self.ignore_internal_cost,
                cleanup_duration: self.cleanup_duration,
                metrics,
                item_size,
                _marker: Default::default(),
            };

            Ok(this)
        }
    }


    /// Cache is a thread-safe implementation of a hashmap with a TinyLFU admission
    /// policy and a Sampled LFU eviction policy. You can use the same Cache instance
    /// from as many threads as you want.
    pub struct Cache<
        K,
        V,
        KH,
        C = DefaultCoster<V>,
        U = DefaultUpdateValidator<V>,
        CB = DefaultCacheCallback<V>,
        PS = RandomState,
        ES = RandomState,
        SS = RandomState,
    > where
        K: Hash + Eq,
        V: Send + Sync + 'static,
        KH: KeyBuilder<K>,
    {
        /// store is the central concurrent hashmap where key-value items are stored.
        pub(crate) store: Arc<ShardedMap<V, U, SS, ES>>,

        /// policy determines what gets let in to the cache and what gets kicked out.
        pub(crate) policy: Arc<LFUPolicy<PS>>,

        /// set_buf is a buffer allowing us to batch/drop Sets during times of high
        /// contention.
        pub(crate) insert_buf_tx: Sender<Item<V>>,

        pub(crate) stop_tx: Sender<()>,

        pub(crate) clear_tx: UnboundedSender<()>,

        pub(crate) callback: Arc<CB>,

        pub(crate) key_to_hash: KH,

        pub(crate) is_closed: AtomicBool,

        pub(crate) coster: Arc<C>,

        pub(crate) ignore_internal_cost: bool,

        pub(crate) cleanup_duration: Duration,

        pub(crate) metrics: Arc<Metrics>,

        pub(crate) item_size: usize,

        _marker: PhantomData<fn(K)>,
    }

    impl<K, V, KH, C, U, CB, PS, ES, SS> Cache<K, V, KH, C, U, CB, PS, ES, SS>
        where
            K: Hash + Eq,
            V: Send + Sync + 'static,
            KH: KeyBuilder<K>,
            C: Coster<V>,
            U: UpdateValidator<V>,
            CB: CacheCallback<V>,
            PS: BuildHasher + Clone + 'static,
            ES: BuildHasher + Clone + 'static,
            SS: BuildHasher + Clone + 'static,
    {


        /// `insert` attempts to add the key-value item to the cache. If it returns false,
        /// then the `insert` was dropped and the key-value item isn't added to the cache. If
        /// it returns true, there's still a chance it could be dropped by the policy if
        /// its determined that the key-value item isn't worth keeping, but otherwise the
        /// item will be added and other items will be evicted in order to make room.
        ///
        /// To dynamically evaluate the items cost using the Config.Coster function, set
        /// the cost parameter to 0 and Coster will be ran when needed in order to find
        /// the items true cost.
        pub async fn insert(&self, key: K, val: V, cost: i64) -> bool {
            self.insert_with_ttl(key, val, cost, Duration::ZERO).await
        }

        /// `insert_with_ttl` works like Set but adds a key-value pair to the cache that will expire
        /// after the specified TTL (time to live) has passed. A zero value means the value never
        /// expires, which is identical to calling `insert`.
        pub async fn insert_with_ttl(&self, key: K, val: V, cost: i64, ttl: Duration) -> bool {
            self.insert_in(key, val, cost, ttl, false).await
        }

        /// `insert_if_present` is like `insert`, but only updates the value of an existing key. It
        /// does NOT add the key to cache if it's absent.
        pub async fn insert_if_present(&self, key: K, val: V, cost: i64) -> bool {
            self.insert_in(key, val, cost, Duration::ZERO, true).await
        }

        pub async fn wait(&self) -> Result<(), CacheError> {
            if self.is_closed.load(Ordering::SeqCst) {
                return Ok(());
            }

            let wg = WaitGroup::new();
            let wait_item = Item::Wait(wg.add(1));
            match self.insert_buf_tx
                .send(wait_item)
                .await {
                Ok(_) => Ok(wg.wait().await),
                Err(e) => Err(CacheError::SendError(format!("cache set buf sender: {}", e.to_string()))),
            }
        }

        pub async fn remove(&self, k: &K) {
            if self.is_closed.load(Ordering::SeqCst) {
                return;
            }

            let (index, conflict) = self.key_to_hash.build_key(&k);
            // delete immediately
            let prev = self.store.remove(&index, conflict);

            if let Some(prev) = prev {
                self.callback.on_exit(Some(prev.value.into_inner()));
            }
            // If we've set an item, it would be applied slightly later.
            // So we must push the same item to `setBuf` with the deletion flag.
            // This ensures that if a set is followed by a delete, it will be
            // applied in the correct order.
            let _ = self.insert_buf_tx.send(Item::delete(index, conflict)).await;
        }

        /// `close` stops all threads and closes all channels.
        pub async fn close(&self) -> Result<(), CacheError> {
            if self.is_closed.load(Ordering::SeqCst) {
                return Ok(());
            }

            self.clear()?;
            // Block until processItems thread is returned
            self.stop_tx.send(()).await.map_err(|e| CacheError::SendError(format!("fail to send stop signal to working thread, {}", e)))?;
            self.policy.close().await?;
            self.is_closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn insert_in(&self, key: K, val: V, cost: i64, ttl: Duration, only_update: bool) -> bool {
            if self.is_closed.load(Ordering::SeqCst) {
                return false;
            }

            if let Some((index, item)) = self.update(key, val, cost, ttl, only_update) {
                let is_update = item.is_update();
                // Attempt to send item to policy.
                tokio::select! {
                    res = self.insert_buf_tx.send(item) => res.map_or_else(|_| {
                       if is_update {
                            // Return true if this was an update operation since we've already
                            // updated the store. For all the other operations (set/delete), we
                            // return false which means the item was not inserted.
                            true
                        } else {
                            self.metrics.add(MetricType::DropSets, index, 1);
                            false
                        }
                    }, |_| true),
                    else => {
                        if is_update {
                            // Return true if this was an update operation since we've already
                            // updated the store. For all the other operations (set/delete), we
                            // return false which means the item was not inserted.
                            true
                        } else {
                            self.metrics.add(MetricType::DropSets, index, 1);
                            false
                        }
                    }
                }
            } else {
                false
            }
        }
    }

    impl<'a, V, C, U, CB, PS, ES, SS> CacheCleaner<'a, V, C, U, CB, PS, ES, SS>
        where
            V: Send + Sync + 'static,
            C: Coster<V>,
            U: UpdateValidator<V>,
            CB: CacheCallback<V>,
            PS: BuildHasher + Clone + 'static,
            ES: BuildHasher + Clone + 'static,
            SS: BuildHasher + Clone + 'static,
    {
        pub(crate) async fn clean(mut self) -> Result<(), CacheError> {
            loop {
                tokio::select! {
                    // clear out the insert buffer channel.
                    Some(item) = self.processor.insert_buf_rx.recv() => {
                        self.handle_item(item);
                    },
                    else => return Ok(()),
                }
            }
        }
    }

    pub(crate) struct CacheProcessor<V, C, U, CB, PS, ES, SS>
        where
            V: Send + Sync + 'static,
            C: Coster<V>,
            U: UpdateValidator<V>,
            CB: CacheCallback<V>,
            PS: BuildHasher + Clone + 'static,
            ES: BuildHasher + Clone + 'static,
            SS: BuildHasher + Clone + 'static,
    {
        pub(crate) insert_buf_rx: Receiver<Item<V>>,
        pub(crate) stop_rx: Receiver<()>,
        pub(crate) clear_rx: UnboundedReceiver<()>,
        pub(crate) metrics: Arc<Metrics>,
        pub(crate) store: Arc<ShardedMap<V, U, SS, ES>>,
        pub(crate) policy: Arc<LFUPolicy<PS>>,
        pub(crate) start_ts: HashMap<u64, Time>,
        pub(crate) num_to_keep: usize,
        pub(crate) callback: Arc<CB>,
        pub(crate) coster: Arc<C>,
        pub(crate) ignore_internal_cost: bool,
        pub(crate) item_size: usize,
    }

    impl<V, C, U, CB, PS, ES, SS> CacheProcessor<V, C, U, CB, PS, ES, SS>
        where
            V: Send + Sync + 'static,
            C: Coster<V>,
            U: UpdateValidator<V>,
            CB: CacheCallback<V>,
            PS: BuildHasher + Clone + 'static,
            ES: BuildHasher + Clone + 'static,
            SS: BuildHasher + Clone + 'static,
    {
        pub fn spawn(
            num_to_keep: usize,
            ignore_internal_cost: bool,
            cleanup_duration: Duration,
            store: Arc<ShardedMap<V, U, SS, ES>>,
            policy: Arc<LFUPolicy<PS>>,
            insert_buf_rx: Receiver<Item<V>>,
            stop_rx: Receiver<()>,
            clear_rx: UnboundedReceiver<()>,
            metrics: Arc<Metrics>,
            coster: Arc<C>,
            callback: Arc<CB>,
        ) -> JoinHandle<Result<(), CacheError>> {

            let item_size = store.item_size();
            let mut this = Self {
                insert_buf_rx,
                stop_rx,
                clear_rx,
                metrics,
                store,
                policy,
                start_ts: HashMap::<u64, Time>::new(),
                num_to_keep,
                callback,
                ignore_internal_cost,
                coster,
                item_size,
            };

            tokio::spawn(async move {
                let cleanup_timer = sleep(cleanup_duration);
                tokio::pin!(cleanup_timer);

                loop {
                    tokio::select! {
                        item = this.insert_buf_rx.recv() => {
                            let _ = this.handle_insert_event(item)?;
                        }
                        _ = &mut cleanup_timer => {
                            cleanup_timer.as_mut().reset(Instant::now() + cleanup_duration);
                            let _ = this.handle_cleanup_event()?;
                        },
                        Some(_) = this.clear_rx.recv() => {
                            let _ = CacheCleaner::new(&mut this).clean().await?;
                        },
                        _ = this.stop_rx.recv() => return this.handle_close_event(),
                    }
                }
            })
        }

        #[inline]
        fn handle_close_event(&mut self) -> Result<(), CacheError> {
            self.insert_buf_rx.close();
            self.clear_rx.close();
            self.stop_rx.close();
            Ok(())
        }

        #[inline]
        fn handle_insert_event(&mut self, res: Option<Item<V>>) -> Result<(), CacheError> {
            res.map(|item| self.handle_item(item))
                .ok_or(CacheError::RecvError(format!("fail to receive msg from insert buffer")))
        }

        #[inline]
        fn handle_cleanup_event(&mut self) -> Result<(), CacheError> {
            self.store
                .cleanup(self.policy.clone())
                .into_iter()
                .for_each(|victim| {
                    self.prepare_evict(&victim);
                    self.callback.on_evict(victim);
                });
            Ok(())
        }
    }
}