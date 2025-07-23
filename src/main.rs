use std::collections::HashSet;

use chrono::{Datelike, Days, NaiveDate, NaiveTime, TimeDelta, Utc, Weekday};
use chrono_tz::Canada::Eastern;
use clap::Parser;
use gtfs_structures::{Exception, Gtfs, PickupDropOffType};
use reqwest::Client;
use tokio::join;

const STATIC_URL: &str = "https://exo.quebec/xdata/trains/google_transit.zip";
const REALTIME_URL: &str =
    "https://exo.chrono-saeiv.com/api/opendata/v1/trains/tripupdate?token=<token>";
const DAY_TRANSITION: NaiveTime = NaiveTime::from_hms_opt(2, 0, 0).unwrap();

#[derive(Parser, Debug)]
#[command(name = "train_display")]
#[command(about = "Work in progress", long_about = None)]
struct Cli {
    station: String,
}

fn service_ids_for(gtfs: &Gtfs, date: NaiveDate) -> Vec<String> {
    let weekday = date.weekday();
    let mut valid_ids = gtfs
        .calendar
        .iter()
        .filter(|(_service_id, service_calendar)| {
            let correct_weekday = match weekday {
                Weekday::Mon => service_calendar.monday,
                Weekday::Tue => service_calendar.tuesday,
                Weekday::Wed => service_calendar.wednesday,
                Weekday::Thu => service_calendar.thursday,
                Weekday::Fri => service_calendar.friday,
                Weekday::Sat => service_calendar.saturday,
                Weekday::Sun => service_calendar.sunday,
            };
            let correct_date =
                service_calendar.start_date <= date && date <= service_calendar.end_date;
            correct_weekday && correct_date
        })
        .map(|(service_id, _service_calendar)| service_id.clone())
        .collect::<HashSet<_>>();
    for (service_id, calendar_dates) in gtfs.calendar_dates.iter() {
        for calendar_date in calendar_dates {
            if calendar_date.date != date {
                continue;
            }
            match calendar_date.exception_type {
                Exception::Added => valid_ids.insert(service_id.clone()),
                Exception::Deleted => valid_ids.remove(service_id),
            };
        }
    }
    valid_ids.into_iter().collect()
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let station_name = args.station;

    let client = Client::new();

    let (gtfs_static, realtime) = join!(
        gtfs_structures::GtfsReader::default().read_from_url_async(STATIC_URL),
        client.get(REALTIME_URL).send()
    );

    let gtfs_static = gtfs_static.expect("No gtfs static");

    let realtime_data = {
        let Ok(response) = realtime else {
            return println!("{:?}", realtime.unwrap_err());
        };
        let bytes = response.bytes().await.unwrap();
        let realtime_data: Result<gtfs_realtime::FeedMessage, prost::DecodeError> =
            prost::Message::decode(bytes.as_ref());
        let Ok(data) = realtime_data else {
            return println!("{:?}", realtime_data.unwrap_err());
        };
        data
    };

    dbg!(&realtime_data);

    let stop_ids: Vec<String> = gtfs_static
        .stops
        .iter()
        .filter(|(_id, stop)| (stop.name.clone().is_some_and(|name| name == station_name)))
        .map(|(id, _stop)| id.into())
        .collect();

    if stop_ids.is_empty() {
        panic!("Station name not found!")
    }

    let current_datetime = Utc::now().with_timezone(&Eastern);
    let today = current_datetime.date_naive();
    let current_time = current_datetime.time();
    let current_naive = today.and_time(current_time);

    let yesterday = today
        .checked_sub_days(Days::new(1))
        .expect("Before common era!");
    let tomorrow = today
        .checked_add_days(Days::new(1))
        .expect("After common era!");

    // iter of (trip_id, departure_time)
    let mut valid_stops = gtfs_static
        .trips
        .iter()
        .flat_map(|(trip_id, trip)| {
            trip.stop_times
                .iter()
                // stops at this station for boarding
                .filter(|stop_time| {
                    stop_ids.contains(&stop_time.stop.id)
                        && stop_time.pickup_type != PickupDropOffType::NotAvailable
                })
                // Select relevant time ranges
                .map(|stop_time| {
                    [yesterday, today, tomorrow]
                        .iter()
                        .filter(|&date| {
                            service_ids_for(&gtfs_static, *date).contains(&trip.service_id)
                        })
                        .map(|date| {
                            (
                                trip_id.clone(),
                                date.and_hms_opt(0, 0, 0)
                                    .unwrap()
                                    .checked_add_signed(TimeDelta::seconds(
                                        stop_time.departure_time.expect("no departure_time").into(),
                                    ))
                                    .expect("After common era!"),
                                trip.trip_headsign.clone().expect("No headsign"),
                            )
                        })
                        .collect::<Vec<_>>()
                })
        })
        .flatten()
        .map(|(trip_id, mut time, headsign)| {
            let delay = realtime_data.entity.iter().find_map(|entity| {
                let update = entity.trip_update.clone()?;
                let id = update.trip.trip_id?;
                if trip_id != id {
                    return None;
                };
                update.stop_time_update.iter().find_map(|stop| {
                    stop_ids
                        .contains(&(stop.stop_id.clone()?))
                        .then(|| stop.departure.iter().find_map(|event| event.delay))
                })
            });

            match delay.flatten() {
                None => (),
                Some(d) => {
                    time = time
                        .checked_add_signed(
                            TimeDelta::new(dbg!(d).into(), 0).expect("Invalid time delta"),
                        )
                        .expect("Time delta add error")
                }
            }
            (trip_id, time, headsign)
        })
        .filter(|(_id, time, _headsign)| {
            *time >= current_naive
                && *time
                    // In the morning, wait until DAY_TRANSITION to show the trains for the day.
                    <= (if current_time > DAY_TRANSITION {
                        tomorrow.and_time(DAY_TRANSITION)
                    } else {
                        today.and_time(DAY_TRANSITION)
                    })
        })
        .collect::<Vec<_>>();

    valid_stops
        .sort_by(|(_id_a, time_a, _headsign_a), (_id_b, time_b, _headsign_b)| time_a.cmp(time_b));

    dbg!(&valid_stops, valid_stops.len());
}
