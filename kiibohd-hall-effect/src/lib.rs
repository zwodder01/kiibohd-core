/* Copyright (C) 2021 by Jacob Alexander
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 */

// ----- Modules -----

#![no_std]

mod rawlookup;
mod test;

// ----- Crates -----

use heapless::{ArrayLength, Vec};
use log::trace;
use typenum::Unsigned;

// TODO Use features to determine which lookup table to use
use rawlookup::MODEL;

// ----- Sense Data -----

/// Calibration status indicates if a sensor position is ready to send
/// analysis for a particular key.
#[repr(C)]
#[derive(Clone, Debug, PartialEq)]
pub enum CalibrationStatus {
    NotReady = 0,                 // Still trying to determine status (from power-on)
    SensorMissing = 1,            // ADC value at 0
    SensorBroken = 2, // Reading higher than ADC supports (invalid), or magnet is too strong
    MagnetDetected = 3, // Magnet detected, min calibrated, positive range
    MagnetWrongPoleOrMissing = 4, // Magnet detected, wrong pole direction
    MagnetTooStrong = 5, // Magnet overpowers sensor
    MagnetTooWeak = 6, // Magnet not strong enough (or removed)
    InvalidIndex = 7, // Invalid index
}

#[derive(Clone, Debug)]
pub enum SensorError {
    CalibrationError(SenseData),
    FailedToResize(usize),
    InvalidSensor(usize),
}

/// Calculations:
///  d = linearized(adc sample) --> distance
///  v = (d - d_prev) / 1       --> velocity
///  a = (v - v_prev) / 2       --> acceleration
///  j = (a - a_prev) / 3       --> jerk
///
/// These calculations assume constant time delta of 1
#[repr(C)]
#[derive(Clone, Debug)]
pub struct SenseAnalysis {
    raw: u16,          // Raw ADC reading
    distance: i16,     // Distance value (lookup + min/max alignment)
    velocity: i16,     // Velocity calculation (*)
    acceleration: i16, // Acceleration calculation (*)
    jerk: i16,         // Jerk calculation (*)
}

impl SenseAnalysis {
    /// Using the raw value do calculations
    /// Requires the previous analysis
    pub fn new(raw: u16, data: &SenseData) -> SenseAnalysis {
        // Do raw lookup (we've already checked the bounds)
        let initial_distance = MODEL[raw as usize];

        // Min/max adjustment
        let distance_offset = match data.cal {
            CalibrationStatus::MagnetDetected => {
                // Subtract the min lookup
                // Lookup table has negative values for unexpectedly
                // small values (greater than sensor center)
                MODEL[data.stats.min as usize]
            }
            _ => {
                // Invalid reading
                return SenseAnalysis::null();
            }
        };
        let distance = initial_distance - distance_offset;
        let velocity = distance - data.analysis.distance; // / 1
        let acceleration = (velocity - data.analysis.velocity) / 2;
        // NOTE: To use jerk, the compile-time thresholds will need to be
        //       multiplied by 3 (to account for the missing / 3)
        let jerk = acceleration - data.analysis.acceleration;
        SenseAnalysis {
            raw,
            distance,
            velocity,
            acceleration,
            jerk,
        }
    }

    /// Null entry
    pub fn null() -> SenseAnalysis {
        SenseAnalysis {
            raw: 0,
            distance: 0,
            velocity: 0,
            acceleration: 0,
            jerk: 0,
        }
    }
}

/// Stores incoming raw samples
#[repr(C)]
#[derive(Clone, Debug)]
pub struct RawData {
    scratch_samples: u8,
    scratch: u32,
}

impl RawData {
    fn new() -> RawData {
        RawData {
            scratch_samples: 0,
            scratch: 0,
        }
    }

    /// Adds to the internal scratch location
    /// Designed to accumulate until a set number of readings added
    /// SC: specifies the number of scratch samples until ready to average
    ///     Should be a power of two (1, 2, 4, 8, 16...) for the compiler to
    ///     optimize.
    fn add<SC: Unsigned>(&mut self, reading: u16) -> Option<u16> {
        self.scratch += reading as u32;
        self.scratch_samples += 1;
        trace!(
            "Reading: {}  Sample: {}/{}",
            reading,
            self.scratch_samples,
            <SC>::to_u8()
        );

        if self.scratch_samples == <SC>::U8 {
            let val = self.scratch / <SC>::U32;
            self.scratch = 0;
            self.scratch_samples = 0;
            Some(val as u16)
        } else {
            None
        }
    }
}

/// Sense stats include statistically information about the sensor data
#[repr(C)]
#[derive(Clone, Debug)]
pub struct SenseStats {
    pub min: u16,     // Minimum raw value (reset when out of calibration)
    pub max: u16,     // Maximum raw value (reset when out of calibration)
    pub samples: u32, // Total number of samples (does not reset)
}

impl SenseStats {
    fn new() -> SenseStats {
        SenseStats {
            min: 0xFFFF,
            max: 0x0000,
            samples: 0,
        }
    }

    /// Reset, resettable stats (e.g. min, max, but not samples)
    fn reset(&mut self) {
        self.min = 0xFFFF;
        self.max = 0x0000;
    }
}

/// Sense data is store per ADC source element (e.g. per key)
/// The analysis is stored in a queue, where old values expire out
/// min/max is used to handle offsets from the distance lookups
/// Higher order calculations assume a constant unit of time between measurements
/// Any division is left to compile-time comparisions as it's not necessary
/// to actually compute the final higher order values in order to make a decision.
/// This diagram can give a sense of how the incoming data is used.
/// The number represents the last ADC sample required to calculate the value.
///
/// ```text,ignore
///
///            4  5 ... <- Jerk (e.g. m/2^3)
///          / | /|
///         3  4  5 ... <- Acceleration (e.g. m/2^2)
///       / | /| /|
///      2  3  4  5 ... <- Velocity (e.g. m/s)
///    / | /| /| /|
///   1  2  3  4  5 ... <- Distance (e.g. m)
///  ----------------------
///   1  2  3  4  5 ... <== ADC Averaged Sample
///
/// ```
///
/// Distance     => Min/Max adjusted lookup
/// Velocity     => (d_current - d_previous) / 1 (constant time)
///                 There is 1 time unit between samples 1 and 2
/// Acceleration => (v_current - v_previous) / 2 (constant time)
///                 There are 2 time units between samples 1 and 3
/// Jerk         => (a_current - a_previous) / 3 (constant time)
///                 There are 3 time units between samples 1 and 4
///
/// NOTE: Division is computed at compile time for jerk (/ 3)
///
/// Time is simplified to 1 unit (normally sampling will be at a constant time-rate, so this should be somewhat accurate).
///
/// A variety of thresholds are used during calibration and normal operating modes.
/// These values are generics as there's no reason to store each of the thresholds at runtime for
/// each sensor (wastes precious sram per sensor).
///
/// Normal Mode:
/// * MMT: Min Magnet Threshold (any lower and calibration is reset)
/// * MX:  Max sensor value
/// Calibration Mode:
/// * MNOK: Min valid calibration (Wrong magnet direction; wrong pole, less than a specific value)
/// * MXOK: Max valid calibration (Bad Sensor threshold; sensor is bad if reading is higher than this value)
/// * NS: No sensor detected (less than a specific value)
#[derive(Clone, Debug)]
pub struct SenseData {
    pub analysis: SenseAnalysis,
    pub cal: CalibrationStatus,
    pub data: RawData,
    pub stats: SenseStats,
}

impl SenseData {
    pub fn new() -> SenseData {
        SenseData {
            analysis: SenseAnalysis::null(),
            cal: CalibrationStatus::NotReady,
            data: RawData::new(),
            stats: SenseStats::new(),
        }
    }

    /// Acculumate a new sensor reading
    /// Once the required number of samples is retrieved, do analysis
    /// Analysis does a few more addition, subtraction and comparisions
    /// so it's a more expensive operation.
    fn add<
        SC: Unsigned,
        MMT: Unsigned,
        MX: Unsigned,
        MNOK: Unsigned,
        MXOK: Unsigned,
        NS: Unsigned,
    >(
        &mut self,
        reading: u16,
    ) -> Result<Option<&SenseAnalysis>, SensorError> {
        // Add value to accumulator
        if let Some(data) = self.data.add::<SC>(reading) {
            // Check min/max values
            if data > self.stats.max {
                self.stats.max = data;
            }
            if data < self.stats.min {
                self.stats.min = data;
            }

            // Check calibration
            self.cal = self.check_calibration::<MMT, MX, MNOK, MXOK, NS>(data);
            trace!("Reading: {}  Cal: {:?}", reading, self.cal);
            match self.cal {
                CalibrationStatus::MagnetDetected => {}
                // Don't bother doing calculations if magnet+sensor isn't ready
                _ => {
                    // Reset min/max
                    self.stats.reset();
                    // Clear analysis, only set raw
                    self.analysis = SenseAnalysis::null();
                    self.analysis.raw = data;
                    return Err(SensorError::CalibrationError(self.clone()));
                }
            }

            // Calculate new analysis (requires previous results + min/max)
            self.analysis = SenseAnalysis::new(data, &self);
            Ok(Some(&self.analysis))
        } else {
            Ok(None)
        }
    }

    /// Update calibration state
    /// Calibration is different depending on whether or not we've already been successfully
    /// calibrated. Gain and offset are set differently depending on whether the sensor has been
    /// calibrated. Uncalibrated sensors run at a lower gain to gather more details around voltage
    /// limits. Wherease calibrated sensors run at higher gain (and likely an offset) to maximize
    /// the voltage range of the desired sensor range.
    /// NOTE: This implementation (currently) only works for a single magnet pole of a bipolar sensor.
    fn check_calibration<
        MMT: Unsigned,
        MX: Unsigned,
        MNOK: Unsigned,
        MXOK: Unsigned,
        NS: Unsigned,
    >(
        &self,
        data: u16,
    ) -> CalibrationStatus {
        // Determine calibration state
        match self.cal {
            // Normal Mode
            CalibrationStatus::MagnetDetected => {
                // Determine if value is too high/overpowers ADC+Gain (recalibrate)
                if data >= <MX>::U16 - 1 {
                    return CalibrationStatus::MagnetTooStrong;
                }
                // Determine if value is too low (recalibrate)
                if data < <MMT>::U16 {
                    return CalibrationStatus::MagnetTooWeak;
                }

                CalibrationStatus::MagnetDetected
            }
            // Calibration Mode
            _ => {
                // Value too high, likely a bad sensor or bad soldering on the pcb
                // Magnet may also be too strong.
                if data > <MXOK>::U16 {
                    if self.cal == CalibrationStatus::MagnetTooStrong {
                        return CalibrationStatus::MagnetTooStrong;
                    }
                    return CalibrationStatus::SensorBroken;
                }
                // No sensor detected
                if data < <NS>::U16 {
                    return CalibrationStatus::SensorMissing;
                }
                // Wrong pole (or magnet may be too weak)
                if data < <MNOK>::U16 {
                    if self.cal == CalibrationStatus::MagnetTooWeak {
                        return CalibrationStatus::MagnetTooWeak;
                    }
                    return CalibrationStatus::MagnetWrongPoleOrMissing;
                }

                CalibrationStatus::MagnetDetected
            }
        }
    }
}

impl Default for SenseData {
    fn default() -> Self {
        SenseData::new()
    }
}

// ----- Hall Effect Interface ------

pub struct Sensors<S: ArrayLength<SenseData>> {
    sensors: Vec<SenseData, S>,
}

impl<S: ArrayLength<SenseData>> Sensors<S> {
    /// Initializes full Sensor array
    /// Only fails if static allocation fails (very unlikely)
    pub fn new() -> Result<Sensors<S>, SensorError> {
        let mut sensors = Vec::new();
        if sensors.resize_default(<S>::to_usize()).is_err() {
            Err(SensorError::FailedToResize(<S>::to_usize()))
        } else {
            Ok(Sensors { sensors })
        }
    }

    pub fn add<
        SC: Unsigned,
        MMT: Unsigned,
        MX: Unsigned,
        MNOK: Unsigned,
        MXOK: Unsigned,
        NS: Unsigned,
    >(
        &mut self,
        index: usize,
        reading: u16,
    ) -> Result<Option<&SenseAnalysis>, SensorError> {
        trace!("Index: {}  Reading: {}", index, reading);
        if index < self.sensors.len() {
            self.sensors[index].add::<SC, MMT, MX, MNOK, MXOK, NS>(reading)
        } else {
            Err(SensorError::InvalidSensor(index))
        }
    }

    pub fn get_data(&self, index: usize) -> Result<&SenseData, SensorError> {
        if index < self.sensors.len() {
            if self.sensors[index].cal == CalibrationStatus::NotReady {
                Err(SensorError::CalibrationError(self.sensors[index].clone()))
            } else {
                Ok(&self.sensors[index])
            }
        } else {
            Err(SensorError::InvalidSensor(index))
        }
    }
}
