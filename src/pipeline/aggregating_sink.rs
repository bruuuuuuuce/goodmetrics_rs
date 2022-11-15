use std::{
    cmp::{max, min},
    collections::{BTreeMap, HashMap},
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Mutex,
    },
};

use crate::{
    allocator::MetricsRef,
    types::{self, Dimension, Measurement, Name},
};

use super::Sink;

// User-named metrics
type MetricsMap = HashMap<Name, DimensionedMeasurementsMap>;
// A metrics measurement family is grouped first by its dimension position
type DimensionedMeasurementsMap = HashMap<DimensionPosition, MeasurementAggregationMap>;
// A dimension position is a unique set of dimensions.
// If a measurement has (1) the same metric name, (2) the same dimensions and (3) the same measurement name as another measurement,
// it is the same measurement and they should be aggregated together.
type DimensionPosition = BTreeMap<Name, Dimension>;
// Within the dimension position there is a collection of named measurements; we'll store the aggregated view of these
type MeasurementAggregationMap = HashMap<Name, Aggregation>;

type Histogram = HashMap<i64, u64>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StatisticSet {
    min: i64,
    max: i64,
    sum: i64,
    count: u64,
}
impl Default for StatisticSet {
    fn default() -> Self {
        Self {
            min: i64::MAX,
            max: i64::MIN,
            sum: 0,
            count: 0,
        }
    }
}
impl StatisticSet {
    fn accumulate<T: Into<i64>>(&mut self, value: T) {
        let v: i64 = value.into();
        self.min = min(v, self.min);
        self.max = max(v, self.max);
        self.sum += v;
        self.count += 1;
    }
}

trait HistogramAccumulate {
    fn accumulate<T: Into<i64>>(&mut self, value: T);
}
impl HistogramAccumulate for Histogram {
    fn accumulate<T: Into<i64>>(&mut self, value: T) {
        let v = value.into();
        let b = bucket_10_2_sigfigs(v);
        self.insert(b, self[&b] + 1);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Aggregation {
    Histogram(Histogram),
    StatisticSet(StatisticSet),
}

pub struct AggregatingSink<TMetricsRef> {
    map: Mutex<MetricsMap>,
    sender: SyncSender<TMetricsRef>,
    receiver: Receiver<TMetricsRef>,
}

impl<TMetricsRef> Default for AggregatingSink<TMetricsRef>
where
    TMetricsRef: MetricsRef,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<TMetricsRef> AggregatingSink<TMetricsRef>
where
    TMetricsRef: MetricsRef,
{
    pub fn new_with_bound(bound: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(bound);
        AggregatingSink {
            map: Mutex::new(MetricsMap::default()),
            sender,
            receiver,
        }
    }

    pub fn new() -> Self {
        Self::new_with_bound(1024)
    }

    // Consume a thread to process metrics aggregation (async support will come separately)
    pub fn run_aggregator_forever(&self) {
        while let Ok(sunk_metrics_ref) = self.receiver.recv() {
            self.update_metrics_map(sunk_metrics_ref);
        }
    }

    fn update_metrics_map(&self, mut sunk_metrics: TMetricsRef)
    where
        TMetricsRef: MetricsRef,
    {
        let mut map = self.map.lock().expect("must be able to access metrics map");
        let dimensioned_measurements_map: &mut DimensionedMeasurementsMap =
            map.entry(sunk_metrics.metrics_name.clone()).or_default();
        let position: DimensionPosition = sunk_metrics.dimensions.drain().collect();
        let measurements_map: &mut MeasurementAggregationMap =
            dimensioned_measurements_map.entry(position).or_default();
        sunk_metrics
            .measurements
            .drain()
            .for_each(|(name, measurement)| match measurement {
                Measurement::Observation(observation) => {
                    accumulate_statisticset(measurements_map, name, observation);
                }
                Measurement::Distribution(distribution) => {
                    accumulate_distribution(measurements_map, name, distribution);
                }
            });
    }
}

fn accumulate_distribution(
    measurements_map: &mut HashMap<Name, Aggregation>,
    name: Name,
    distribution: types::Distribution,
) {
    match measurements_map
        .entry(name)
        .or_insert_with(|| Aggregation::Histogram(Histogram::default()))
    {
        Aggregation::StatisticSet(_s) => {
            log::error!("conflicting measurement and distribution name")
        }
        Aggregation::Histogram(histogram) => {
            match distribution {
                types::Distribution::I64(i) => {
                    histogram
                        .entry(bucket_10_2_sigfigs(i))
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                types::Distribution::I32(i) => {
                    histogram
                        .entry(bucket_10_2_sigfigs(i.into()))
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                types::Distribution::U64(i) => {
                    histogram
                        .entry(bucket_10_2_sigfigs(i as i64))
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                types::Distribution::U32(i) => {
                    histogram
                        .entry(bucket_10_2_sigfigs(i.into()))
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                types::Distribution::Collection(collection) => {
                    collection.iter().for_each(|i| {
                        histogram
                            .entry(bucket_10_2_sigfigs(*i as i64))
                            .and_modify(|count| *count += 1)
                            .or_insert(1);
                    });
                }
            };
        }
    }
}

fn accumulate_statisticset(
    measurements_map: &mut HashMap<Name, Aggregation>,
    name: Name,
    observation: types::Observation,
) {
    match measurements_map
        .entry(name)
        .or_insert_with(|| Aggregation::StatisticSet(StatisticSet::default()))
    {
        Aggregation::StatisticSet(statistic_set) => statistic_set.accumulate(observation),
        Aggregation::Histogram(_h) => {
            log::error!("conflicting measurement and distribution name")
        }
    }
}

impl<TMetricsRef> Sink<TMetricsRef> for AggregatingSink<TMetricsRef> {
    fn accept(&self, metrics_ref: TMetricsRef) {
        match self.sender.try_send(metrics_ref) {
            Ok(_) => {}
            Err(error) => {
                log::error!("could not send metrics to channel: {error}")
            }
        }
    }
}

// Base 10 significant-figures bucketing
fn bucket_10<const FIGURES: u32>(value: i64) -> i64 {
    // TODO: use i64.log10 when it's promoted to stable https://github.com/rust-lang/rust/issues/70887
    let power = ((value.abs() as f64).log10().ceil() as i32 - FIGURES as i32).max(0);
    let magnitude = 10_f64.powi(power);

    value.signum()
        // -> truncate off magnitude by dividing it away
        // -> ceil() away from 0 in both directions due to abs
        * (value.abs() as f64 / magnitude).ceil() as i64
        // restore original magnitude raised to the next figure if necessary
        * magnitude as i64
}

fn bucket_10_2_sigfigs(value: i64) -> i64 {
    bucket_10::<2>(value)
}

#[cfg(test)]
mod test {
    use std::collections::{BTreeMap, HashMap};

    use crate::{
        allocator::{always_new_metrics_allocator::AlwaysNewMetricsAllocator, MetricsAllocator},
        metrics::Metrics,
        pipeline::aggregating_sink::{
            bucket_10_2_sigfigs, AggregatingSink, Aggregation, StatisticSet,
        },
        types::{Dimension, Name, Observation},
    };

    #[test_log::test]
    fn test_bucket() {
        assert_eq!(1, bucket_10_2_sigfigs(1));
        assert_eq!(-11, bucket_10_2_sigfigs(-11));

        assert_eq!(99, bucket_10_2_sigfigs(99));
        assert_eq!(100, bucket_10_2_sigfigs(100));
        assert_eq!(110, bucket_10_2_sigfigs(101));
        assert_eq!(110, bucket_10_2_sigfigs(109));
        assert_eq!(110, bucket_10_2_sigfigs(110));
        assert_eq!(120, bucket_10_2_sigfigs(111));

        assert_eq!(8000, bucket_10_2_sigfigs(8000));
        assert_eq!(8800, bucket_10_2_sigfigs(8799));
        assert_eq!(8800, bucket_10_2_sigfigs(8800));
        assert_eq!(8900, bucket_10_2_sigfigs(8801));

        assert_eq!(-8000, bucket_10_2_sigfigs(-8000));
        assert_eq!(-8800, bucket_10_2_sigfigs(-8799));
        assert_eq!(-8800, bucket_10_2_sigfigs(-8800));
        assert_eq!(-8900, bucket_10_2_sigfigs(-8801));
    }

    #[test_log::test]
    fn test_aggregation() {
        let sink: AggregatingSink<Box<Metrics>> = AggregatingSink::new();

        sink.update_metrics_map(get_metrics("a", "dimension", "v", 22));
        sink.update_metrics_map(get_metrics("a", "dimension", "v", 20));

        let map = sink.map.lock().unwrap();
        assert_eq!(
            HashMap::from([(
                Name::from("test"),
                HashMap::from([(
                    BTreeMap::from([(Name::from("a"), Dimension::from("dimension"))]),
                    HashMap::from([(
                        Name::from("v"),
                        Aggregation::StatisticSet(StatisticSet {
                            min: 20,
                            max: 22,
                            sum: 42,
                            count: 2
                        })
                    )])
                )])
            )]),
            *map,
        )
    }

    fn get_metrics(
        dimension_name: impl Into<Name>,
        dimension: impl Into<Dimension>,
        measurement_name: impl Into<Name>,
        measurement: impl Into<Observation>,
    ) -> Box<Metrics> {
        let mut metrics = AlwaysNewMetricsAllocator::default().new_metrics("test");
        metrics.dimension(dimension_name, dimension);
        metrics.measurement(measurement_name, measurement);
        metrics
    }
}
