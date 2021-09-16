// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Modular Timely Dataflow operators that can persist and seal updates in streams.

use std::collections::HashMap;

use persist_types::Codec;

use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::Capability;
use timely::dataflow::operators::FrontierNotificator;
use timely::dataflow::operators::OkErr;
use timely::dataflow::operators::Operator;
use timely::dataflow::{Scope, Stream};
use timely::progress::Antichain;
use timely::{Data as TimelyData, PartialOrder};

use crate::indexed::runtime::StreamWriteHandle;

/// Extension trait for [`Stream`].
pub trait Persist<G: Scope<Timestamp = u64>, K: TimelyData, V: TimelyData> {
    /// Passes through each element of the stream and persists it.
    ///
    /// This does not wait for persistence before passing through the data. We do, however, wait
    /// for data to be persisted before allowing the frontier to advance. In other words, this
    /// operator is holding on to capabilities as long as data belonging to their timestamp is not
    /// persisted.
    ///
    /// Use this together with [`seal`](Seal::seal)/[`conditional_seal`](Seal::conditional_seal)
    /// and [`await_frontier`](AwaitFrontier::await_frontier) if you want to make sure that data only
    /// becomes available downstream when persisted and sealed.
    ///
    /// **Note:** If you need to also replay persisted data when restarting, concatenate the output
    /// of this operator with the output of `replay()`.
    fn persist(
        &self,
        name: &str,
        write: StreamWriteHandle<K, V>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    );
}

impl<G, K, V> Persist<G, K, V> for Stream<G, ((K, V), u64, isize)>
where
    G: Scope<Timestamp = u64>,
    K: TimelyData + Codec,
    V: TimelyData + Codec,
{
    fn persist(
        &self,
        name: &str,
        write: StreamWriteHandle<K, V>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    ) {
        let operator_name = format!("persist({})", name);
        let mut persist_op = OperatorBuilder::new(operator_name.clone(), self.scope());

        let mut input = persist_op.new_input(&self, Pipeline);

        let (mut data_output, data_output_stream) = persist_op.new_output();
        let (mut error_output, error_output_stream) = persist_op.new_output();

        let mut buffer = Vec::new();
        let mut write_futures = HashMap::new();
        let mut input_frontier =
            Antichain::from_elem(<G::Timestamp as timely::progress::Timestamp>::minimum());
        let error_output_port = error_output_stream.name().port;

        persist_op.build(move |_capabilities| {
            move |frontiers| {
                let mut data_output = data_output.activate();
                let mut error_output = error_output.activate();

                // Write out everything and forward, keeping the write futures.
                input.for_each(|cap, data| {
                    data.swap(&mut buffer);

                    let write_future = write.write(buffer.iter().as_ref());

                    let mut session = data_output.session(&cap);
                    session.give_vec(&mut buffer);

                    let write_futures = &mut write_futures
                        .entry(cap.retain_for_output(error_output_port))
                        .or_insert_with(|| Vec::new());
                    write_futures.push(write_future);
                });

                // Block on outstanding writes when the input frontier advances.
                // This way, when the downstream frontier advances, we know that all writes that
                // are before it are done.
                let new_input_frontier = frontiers[0].frontier();
                let progress =
                    !PartialOrder::less_equal(&new_input_frontier, &input_frontier.borrow());

                if !progress {
                    return;
                }

                input_frontier.clear();
                input_frontier.extend(new_input_frontier.into_iter().cloned());
                let contained_times: Vec<_> = write_futures
                    .keys()
                    .filter(|time| !input_frontier.less_equal(time.time()))
                    .cloned()
                    .collect();

                // TODO: Even more pipelining: the operator should yield when the futures
                // are not ready and re-schedule itself using an `Activator`. As it is, we
                // have a synchronization barrier once every second (default timestamping
                // interval).
                // TODO: Potentially move the logic for determining when futures are ready
                // and frontier management into a struct/impl.
                for time in contained_times {
                    let write_futures = write_futures.remove(&time).expect("missing futures");

                    log::trace!(
                        "In {} waiting on write futures for time: {}",
                        &operator_name,
                        time.time()
                    );
                    for future in write_futures {
                        if let Err(e) = future.recv() {
                            let mut session = error_output.session(&time);
                            // TODO: make error retractable? Probably not...
                            session.give((e.to_string(), *time.time(), 1));
                        }
                    }
                    log::trace!(
                        "In {} finished write futures for time: {}",
                        &operator_name,
                        time.time()
                    );
                }
            }
        });

        (data_output_stream, error_output_stream)
    }
}

/// Extension trait for [`Stream`].
pub trait Seal<G: Scope<Timestamp = u64>, K: TimelyData, V: TimelyData> {
    /// Passes through each element of the stream and seals the given collection (the `write`
    /// handle) when the input frontier advances.
    ///
    /// This does not wait for the seal to succeed before passing through the data. We do, however,
    /// wait for the seal to be successful before allowing the frontier to advance. In other words,
    /// this operator is holding on to capabilities as long as seals corresponding to their
    /// timestamp are not done.
    fn seal(
        &self,
        name: &str,
        write: StreamWriteHandle<K, V>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    );

    /// Passes through each element of the stream and seals the given primary and condition
    /// collections, respectively, when their frontier advances. The primary collection is only
    /// sealed up to a time `t` when the condition collection has also been sealed up to `t`.
    ///
    /// This does not wait for the seals to succeed before passing through the data. We do,
    /// however, wait for the seals to be successful before allowing the frontier to advance. In
    /// other words, this operator is holding on to capabilities as long as seals corresponding to
    /// their timestamp are not done.
    fn conditional_seal<K2, V2>(
        &self,
        name: &str,
        condition_input: &Stream<G, ((K2, V2), u64, isize)>,
        primary_write: StreamWriteHandle<K, V>,
        condition_write: StreamWriteHandle<K2, V2>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    )
    where
        K2: TimelyData + Codec,
        V2: TimelyData + Codec;
}

impl<G, K, V> Seal<G, K, V> for Stream<G, ((K, V), u64, isize)>
where
    G: Scope<Timestamp = u64>,
    K: TimelyData + Codec,
    V: TimelyData + Codec,
{
    fn seal(
        &self,
        name: &str,
        write: StreamWriteHandle<K, V>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    ) {
        let operator_name = format!("seal({})", name);
        let mut seal_op = OperatorBuilder::new(operator_name.clone(), self.scope());

        let mut data_input = seal_op.new_input(&self, Pipeline);

        let (mut data_output, data_output_stream) = seal_op.new_output();
        let (mut error_output, error_output_stream) = seal_op.new_output();

        let mut data_buffer = Vec::new();
        let mut input_frontier =
            Antichain::from_elem(<G::Timestamp as timely::progress::Timestamp>::minimum());
        let mut capabilities = Antichain::<Capability<u64>>::new();
        let error_output_port = error_output_stream.name().port;

        // We only seal from one worker because sealing from multiple workers could lead to a race
        // conditions where one worker seals up to time `t` while another worker is still trying to
        // write data with timestamps that are not beyond `t`.
        //
        // Upstream persist() operators will only advance their frontier when writes are succesful.
        // With timely progress tracking we are therefore sure that when the frontier advances for
        // worker 0, it has advanced to at least that point for all upstream operators.
        //
        // Alternative solutions would be to "teach" persistence to work with seals from multiple
        // workers, or to use a non-timely solution for keeping track of outstanding write
        // capabilities.
        let active_seal_operator = self.scope().index() == 0;

        seal_op.build(move |_capabilities| {
            move |frontiers| {
                let mut data_output = data_output.activate();
                let mut error_output = error_output.activate();

                // Pass through all data.
                data_input.for_each(|cap, data| {
                    data.swap(&mut data_buffer);

                    let mut session = data_output.session(&cap);
                    session.give_vec(&mut data_buffer);

                    // We only need capabilities for reporting errors, which we only need to do
                    // when we're the active operator.
                    if active_seal_operator {
                        capabilities.insert(cap.retain_for_output(error_output_port));
                    }
                });

                if !active_seal_operator {
                    return;
                }

                // Seal if/when the frontier advances.
                let new_input_frontier = frontiers[0].frontier();
                let progress =
                    !PartialOrder::less_equal(&new_input_frontier, &input_frontier.borrow());

                if !progress {
                    return;
                }

                // Only try and seal if we have seen some data. Otherwise, we wouldn't have
                // a capability that allows us to emit errors.
                if let Some(err_cap) = capabilities.get(0) {
                    for frontier_element in new_input_frontier.iter() {
                        // Only seal if this element of the new input frontier truly
                        // represents progress. With Antichain<u64>, this will always be
                        // the case, but antichains of types with a different partial order
                        // can have frontier progress and have some elements that don't
                        // represent progress.
                        if !input_frontier.less_than(frontier_element) {
                            continue;
                        }

                        log::trace!("Sealing {} up to {}", &operator_name, frontier_element);

                        // TODO: Don't block on the seal. Instead, we should yield from the
                        // operator and/or find some other way to wait for the seal to succeed.
                        let result = write.seal(*frontier_element).recv();
                        if let Err(e) = result {
                            log::error!(
                                "Error sealing {} up to {}: {:?}",
                                &operator_name,
                                frontier_element,
                                e
                            );

                            let mut session = error_output.session(err_cap);
                            // TODO: Make error retractable? Probably not...
                            session.give((e.to_string(), *err_cap.time(), 1));
                        }
                    }
                }

                input_frontier.clear();
                input_frontier.extend(new_input_frontier.into_iter().cloned());

                // If we didn't yet receive any data we won't have capabilities yet.
                if !capabilities.is_empty() {
                    // Try and maintain the least amount of capabilities. In our case, where
                    // the timestamp is u64, this means we only ever keep one capability
                    // because u64 has a total order and the input frontier therefore only ever
                    // contains one element.
                    //
                    // This solution is very generic, though, and will work for the case where
                    // we don't use u64 as the timestamp.
                    let mut new_capabilities = Antichain::new();
                    for time in input_frontier.iter() {
                        if let Some(capability) = capabilities
                            .elements()
                            .iter()
                            .find(|c| c.time().less_equal(time))
                        {
                            new_capabilities.insert(capability.delayed(time));
                        } else {
                            panic!("failed to find capability");
                        }
                    }

                    capabilities = new_capabilities;
                }
            }
        });

        (data_output_stream, error_output_stream)
    }

    fn conditional_seal<K2, V2>(
        &self,
        name: &str,
        condition_input: &Stream<G, ((K2, V2), u64, isize)>,
        primary_write: StreamWriteHandle<K, V>,
        condition_write: StreamWriteHandle<K2, V2>,
    ) -> (
        Stream<G, ((K, V), u64, isize)>,
        Stream<G, (String, u64, isize)>,
    )
    where
        K2: TimelyData + Codec,
        V2: TimelyData + Codec,
    {
        let operator_name = format!("conditional_seal({})", name);
        let mut seal_op = OperatorBuilder::new(operator_name.clone(), self.scope());

        let mut primary_data_input = seal_op.new_input(&self, Pipeline);
        let mut condition_data_input = seal_op.new_input(condition_input, Pipeline);

        let (mut data_output, data_output_stream) = seal_op.new_output();

        let mut primary_data_buffer = Vec::new();
        let mut condition_data_buffer = Vec::new();

        // We only seal from one worker because sealing from multiple workers could lead to a race
        // conditions where one worker seals up to time `t` while another worker is still trying to
        // write data with timestamps that are not beyond `t`.
        //
        // Upstream persist() operators will only advance their frontier when writes are succesful.
        // With timely progress tracking we are therefore sure that when the frontier advances for
        // worker 0, it has advanced to at least that point for all upstream operators.
        //
        // Alternative solutions would be to "teach" persistence to work with seals from multiple
        // workers, or to use a non-timely solution for keeping track of outstanding write
        // capabilities.
        let active_seal_operator = self.scope().index() == 0;

        seal_op.build(move |mut capabilities| {
            let mut primary_notificator = FrontierNotificator::new();
            let mut condition_notificator = FrontierNotificator::new();

            if active_seal_operator {
                let initial_primary_cap = capabilities.pop().expect("missing capability");
                let initial_condition_cap = initial_primary_cap.clone();

                // We need to start with some notify. Otherwise, we would not seal a collection that
                // corresponds to an input that never received any data.
                primary_notificator.notify_at(initial_primary_cap);
                condition_notificator.notify_at(initial_condition_cap);
            }

            move |frontiers| {
                let mut data_output = data_output.activate();

                let frontiers = &[&frontiers[0], &frontiers[1]];
                let mut primary_notificator = primary_notificator.monotonic(frontiers, &None);
                let mut condition_notificator = condition_notificator.monotonic(frontiers, &None);

                // Pass through all data.
                primary_data_input.for_each(|cap, data| {
                    data.swap(&mut primary_data_buffer);

                    let as_result = primary_data_buffer.drain(..).map(|update| Ok(update));

                    let mut session = data_output.session(&cap);
                    session.give_iterator(as_result);

                    if active_seal_operator {
                        // Explicitly seal at the time of received data. We also repeatedly set up
                        // notifies based on the frontier, below, but also using the
                        // time/capability of incoming data might make things more fine-grained.
                        primary_notificator.notify_at(cap.retain());
                    }
                });

                // Consume condition input data but throw it away. We only use this
                // input to track the frontier (to know how far we're sealed up).
                // TODO: There should be a better way for doing this, maybe?
                condition_data_input.for_each(|cap, data| {
                    data.swap(&mut condition_data_buffer);
                    condition_data_buffer.drain(..);

                    if active_seal_operator {
                        // Explicitly seal at the time of received data. We also repeatedly set up
                        // notifies based on the frontier, below, but also using the
                        // time/capability of incoming data might make things more fine-grained.
                        condition_notificator.notify_at(cap.retain());
                    }
                });

                if !active_seal_operator {
                    return;
                }

                (&mut condition_notificator).for_each(|cap, _count, notificator| {
                    log::trace!(
                        "In {}, sealing condition input up to {}...",
                        &operator_name,
                        cap.time(),
                    );

                    // Notify when the frontier advances again.
                    let mut combined_frontier: Antichain<u64> = Antichain::new();
                    combined_frontier.extend(notificator.frontier(0).iter().cloned());
                    combined_frontier.extend(notificator.frontier(1).iter().cloned());
                    for frontier_element in combined_frontier {
                        notificator.notify_at(cap.delayed(&frontier_element));
                    }

                    // TODO: Don't block on the seal. Instead, we should yield from the
                    // operator and/or find some other way to wait for the seal to succeed.
                    let result = condition_write.seal(*cap.time()).recv();

                    log::trace!(
                        "In {}, finished sealing condition input up to {}",
                        &operator_name,
                        cap.time(),
                    );

                    if let Err(e) = result {
                        let mut session = data_output.session(&cap);
                        log::error!(
                            "Error sealing {} (condition) up to {}: {:?}",
                            &operator_name,
                            cap.time(),
                            e
                        );
                        // TODO: make error retractable? Probably not...
                        session.give(Err((e.to_string(), *cap.time(), 1)));
                    }
                });

                (&mut primary_notificator).for_each(|cap, _count, notificator| {
                    log::trace!(
                        "In {}, sealing primary input up to {}...",
                        &operator_name,
                        cap.time(),
                    );

                    // Notify when the frontier advances again.
                    let mut combined_frontier: Antichain<u64> = Antichain::new();
                    combined_frontier.extend(notificator.frontier(0).iter().cloned());
                    combined_frontier.extend(notificator.frontier(1).iter().cloned());
                    for frontier_element in combined_frontier {
                        notificator.notify_at(cap.delayed(&frontier_element));
                    }

                    // TODO: Don't block on the seal. Instead, we should yield from the
                    // operator and/or find some other way to wait for the seal to succeed.
                    let result = primary_write.seal(*cap.time()).recv();

                    log::trace!(
                        "In {}, finished sealing primary input up to {}",
                        &operator_name,
                        cap.time(),
                    );

                    if let Err(e) = result {
                        let mut session = data_output.session(&cap);
                        log::error!(
                            "Error sealing {} (condition) up to {}: {:?}",
                            &operator_name,
                            cap.time(),
                            e
                        );
                        // TODO: make error retractable? Probably not...
                        session.give(Err((e.to_string(), *cap.time(), 1)));
                    }
                });
            }
        });

        // We use a single, multiplexed output instead of dealing with the hassles of managing
        // capabilities for a regular output and an error output for the seal operator.
        let (data_output_stream, error_output_stream) = data_output_stream.ok_err(|x| x);

        (data_output_stream, error_output_stream)
    }
}

/// Extension trait for [`Stream`].
pub trait AwaitFrontier<G: Scope<Timestamp = u64>, D> {
    /// Stashes data until it is no longer beyond the input frontier.
    ///
    /// This is similar, in spirit, to what `consolidate()` does for differential collections and
    /// what `delay()` does for timely streams. However, `consolidate()` does more work than what we
    /// need and `delay()` deals with changing the timestamp while the behaviour we want is to wait for
    /// the frontier to pass. The latter is an implementation detail of `delay()` that is not
    /// advertised in its documentation. We therefore have our own implementation that we control
    /// to be sure we don't break if `delay()` ever changes.
    fn await_frontier(&self, name: &str) -> Stream<G, (D, u64, isize)>;
}

impl<G, D> AwaitFrontier<G, D> for Stream<G, (D, u64, isize)>
where
    G: Scope<Timestamp = u64>,
    D: TimelyData,
{
    // Note: This is mostly a copy of the timely delay() operator without the delaying part.
    fn await_frontier(&self, name: &str) -> Stream<G, (D, u64, isize)> {
        let operator_name = format!("await_frontier({})", name);

        // The values here are Vecs of Vecs. That's how the original timely code does it, to re-use
        // allocations and not have to keep extending a single Vec.
        let mut elements = HashMap::new();

        self.unary_notify(
            Pipeline,
            &operator_name,
            vec![],
            move |input, output, notificator| {
                input.for_each(|time, data| {
                    elements
                        .entry(time.clone())
                        .or_insert_with(|| {
                            notificator.notify_at(time.retain());
                            Vec::new()
                        })
                        .push(data.replace(Vec::new()));
                });

                // For each available notification, send corresponding set.
                notificator.for_each(|time, _, _| {
                    if let Some(mut datas) = elements.remove(&time) {
                        for mut data in datas.drain(..) {
                            output.session(&time).give_vec(&mut data);
                        }
                    } else {
                        panic!("Missing data for time {}", time.time());
                    }
                });
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use timely::dataflow::operators::capture::Extract;
    use timely::dataflow::operators::input::Handle;
    use timely::dataflow::operators::probe::Probe;
    use timely::dataflow::operators::Capture;

    use crate::error::Error;
    use crate::indexed::{ListenEvent, SnapshotExt};
    use crate::mem::MemRegistry;
    use crate::unreliable::UnreliableHandle;

    use super::*;

    #[test]
    fn persist() -> Result<(), Error> {
        let mut registry = MemRegistry::new();

        let p = registry.runtime_no_reentrance()?;
        timely::execute_directly(move |worker| {
            let (mut input, probe) = worker.dataflow(|scope| {
                let (write, _read) = p.create_or_load("1").unwrap();
                let mut input = Handle::new();
                let (ok_stream, _) = input.to_stream(scope).persist("test", write);
                let probe = ok_stream.probe();
                (input, probe)
            });
            for i in 1..=5 {
                input.send(((i.to_string(), ()), i, 1));
            }
            input.advance_to(6);
            while probe.less_than(&6) {
                worker.step();
            }
        });

        let expected = vec![
            (("1".to_string(), ()), 1, 1),
            (("2".to_string(), ()), 2, 1),
            (("3".to_string(), ()), 3, 1),
            (("4".to_string(), ()), 4, 1),
            (("5".to_string(), ()), 5, 1),
        ];

        let p = registry.runtime_no_reentrance()?;
        let (_write, read) = p.create_or_load("1")?;
        assert_eq!(read.snapshot()?.read_to_end()?, expected);

        Ok(())
    }

    #[test]
    fn persist_error_stream() -> Result<(), Error> {
        let mut unreliable = UnreliableHandle::default();
        let p = MemRegistry::new().runtime_unreliable(unreliable.clone())?;

        let (write, _read) = p.create_or_load::<(), ()>("error_stream").unwrap();
        unreliable.make_unavailable();

        let recv = timely::execute_directly(move |worker| {
            let (mut input, probe, err_stream) = worker.dataflow(|scope| {
                let mut input = Handle::new();
                let (_, err_stream) = input.to_stream(scope).persist("test", write);
                let probe = err_stream.probe();
                (input, probe, err_stream.capture())
            });

            input.send((((), ()), 1, 1));
            input.advance_to(1);

            while probe.less_than(&1) {
                worker.step();
            }

            err_stream
        });

        let actual = recv
            .extract()
            .into_iter()
            .flat_map(|(_, xs)| xs.into_iter())
            .collect::<Vec<_>>();

        let expected = vec![(
            "failed to append to unsealed: unavailable: blob set".to_string(),
            0,
            1,
        )];
        assert_eq!(actual, expected);

        Ok(())
    }

    #[test]
    fn seal() -> Result<(), Error> {
        let mut registry = MemRegistry::new();

        let p = registry.runtime_no_reentrance()?;

        timely::execute_directly(move |worker| {
            let (mut input, probe) = worker.dataflow(|scope| {
                let (write, _read) = p.create_or_load("1").unwrap();
                let mut input = Handle::new();
                let (ok_stream, _) = input.to_stream(scope).seal("test", write);
                let probe = ok_stream.probe();
                (input, probe)
            });
            input.send((((), ()), 1, 1));
            input.advance_to(42);
            while probe.less_than(&42) {
                worker.step();
            }
        });

        let p = registry.runtime_no_reentrance()?;
        let (_write, read) = p.create_or_load::<(), ()>("1")?;
        assert_eq!(read.snapshot()?.get_seal(), Antichain::from_elem(42));

        Ok(())
    }

    #[test]
    fn seal_error_stream() -> Result<(), Error> {
        let mut unreliable = UnreliableHandle::default();
        let p = MemRegistry::new().runtime_unreliable(unreliable.clone())?;

        let (write, _read) = p.create_or_load::<(), ()>("error_stream").unwrap();
        unreliable.make_unavailable();

        let recv = timely::execute_directly(move |worker| {
            let (mut input, probe, err_stream) = worker.dataflow(|scope| {
                let mut input = Handle::new();
                let (_, err_stream) = input.to_stream(scope).seal("test", write);
                let probe = err_stream.probe();
                (input, probe, err_stream.capture())
            });

            input.send((((), ()), 1, 1));
            input.advance_to(1);

            while probe.less_than(&1) {
                worker.step();
            }

            err_stream
        });

        let actual = recv
            .extract()
            .into_iter()
            .flat_map(|(_, xs)| xs.into_iter())
            .collect::<Vec<_>>();

        let expected = vec![(
            "failed to commit metadata after appending to unsealed: unavailable: blob set"
                .to_string(),
            0,
            1,
        )];
        assert_eq!(actual, expected);

        Ok(())
    }

    #[test]
    fn conditional_seal() -> Result<(), Error> {
        let mut registry = MemRegistry::new();

        let p = registry.runtime_no_reentrance()?;

        // Setup listens for both collections and record seal events. Afterwards, we will verify
        // that we get the expected seals, in the right order.
        let (_write, primary_read) = p.create_or_load::<(), ()>("primary").unwrap();
        let (_write, condition_read) = p.create_or_load::<(), ()>("condition").unwrap();

        #[derive(Debug, PartialEq, Eq)]
        enum Sealed {
            Primary(u64),
            Condition(u64),
        }

        let (listen_tx, listen_rx) = mpsc::channel();

        {
            let listen_tx = listen_tx.clone();
            let listen_fn = Box::new(move |e| {
                match e {
                    ListenEvent::Sealed(t) => listen_tx.send(Sealed::Primary(t)).unwrap(),
                    _ => panic!("unexpected data"),
                };
                ()
            });

            primary_read.listen(listen_fn)?;
        };
        {
            let listen_fn = Box::new(move |e| {
                match e {
                    ListenEvent::Sealed(t) => listen_tx.send(Sealed::Condition(t)).unwrap(),
                    _ => panic!("unexpected data"),
                };
                ()
            });

            condition_read.listen(listen_fn)?;
        };

        timely::execute_directly(move |worker| {
            let (mut primary_input, mut condition_input, primary_probe, condition_probe) = worker
                .dataflow(|scope| {
                    let (primary_write, _read) = p.create_or_load::<(), ()>("primary").unwrap();
                    let (condition_write, _read) = p.create_or_load::<(), ()>("condition").unwrap();
                    let mut primary_input = Handle::new();
                    let mut condition_input = Handle::new();
                    let primary_stream = primary_input.to_stream(scope);
                    let condition_stream = condition_input.to_stream(scope);
                    let (_, _) = primary_stream.conditional_seal(
                        "test",
                        &condition_stream,
                        primary_write,
                        condition_write,
                    );

                    let primary_probe = primary_stream.probe();
                    let condition_probe = condition_stream.probe();

                    (
                        primary_input,
                        condition_input,
                        primary_probe,
                        condition_probe,
                    )
                });

            // Only send data on the condition input, not on the primary input. This simulates the
            // case where our primary input never sees any data.
            condition_input.send((((), ()), 0, 1));

            primary_input.advance_to(1);
            condition_input.advance_to(1);
            while primary_probe.less_than(&1) {
                worker.step();
            }

            // Pull primary input to 3 already. We're still expecting a seal at 2 for primary,
            // though, when condition advances to 2.
            primary_input.advance_to(3);
            while primary_probe.less_than(&3) {
                worker.step();
            }

            condition_input.advance_to(2);
            while condition_probe.less_than(&2) {
                worker.step();
            }

            condition_input.advance_to(3);
            while condition_probe.less_than(&3) {
                worker.step();
            }
        });

        let actual_seals: Vec<_> = listen_rx.try_iter().collect();

        // Assert that:
        //  a) We don't seal primary when condition has not sufficiently advanced.
        //  b) Condition is sealed before primary for the same timestamp.
        //  c) We seal up, even when never receiving any data.
        assert_eq!(
            vec![
                Sealed::Condition(1),
                Sealed::Primary(1),
                Sealed::Condition(2),
                Sealed::Primary(2),
                Sealed::Condition(3),
                Sealed::Primary(3)
            ],
            actual_seals
        );

        Ok(())
    }

    #[test]
    fn conditional_seal_error_stream() -> Result<(), Error> {
        let mut unreliable = UnreliableHandle::default();
        let p = MemRegistry::new().runtime_unreliable(unreliable.clone())?;

        let (primary_write, _read) = p.create_or_load::<(), ()>("primary").unwrap();
        let (condition_write, _read) = p.create_or_load::<(), ()>("condition").unwrap();
        unreliable.make_unavailable();

        let recv = timely::execute_directly(move |worker| {
            let (mut primary_input, mut condition_input, probe, err_stream) =
                worker.dataflow(|scope| {
                    let mut primary_input = Handle::new();
                    let mut condition_input = Handle::new();
                    let primary_stream = primary_input.to_stream(scope);
                    let condition_stream = condition_input.to_stream(scope);

                    let (_, err_stream) = primary_stream.conditional_seal(
                        "test",
                        &condition_stream,
                        primary_write,
                        condition_write,
                    );

                    let probe = err_stream.probe();
                    (primary_input, condition_input, probe, err_stream.capture())
                });

            primary_input.send((((), ()), 0, 1));
            condition_input.send((((), ()), 0, 1));

            primary_input.advance_to(1);
            condition_input.advance_to(1);

            while probe.less_than(&1) {
                worker.step();
            }

            err_stream
        });

        let actual = recv
            .extract()
            .into_iter()
            .flat_map(|(_, xs)| xs.into_iter())
            .collect::<Vec<_>>();

        let expected = vec![
            (
                "failed to commit metadata after appending to unsealed: unavailable: blob set"
                    .to_string(),
                0,
                1,
            ),
            (
                "failed to commit metadata after appending to unsealed: unavailable: blob set"
                    .to_string(),
                0,
                1,
            ),
            (
                "failed to commit metadata after appending to unsealed: unavailable: blob set"
                    .to_string(),
                1,
                1,
            ),
            (
                "failed to commit metadata after appending to unsealed: unavailable: blob set"
                    .to_string(),
                1,
                1,
            ),
        ];
        assert_eq!(actual, expected);

        Ok(())
    }
}
