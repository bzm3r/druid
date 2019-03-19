// Copyright 2019 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tracing related utility functions.

use std::fs::File;
use xi_trace;

/// Save tracing data to path pointed to by the environment variable TRACE_OUTPUT, using the Trace
/// Viewer format. Save path defaults to `./target/trace_output.trace`. Trace file can be opened
/// with the Chrome browser by visiting the URL `about:tracing`.
pub fn save_trace() {
    use std::env;
    use xi_trace::chrome_trace_dump;

    let all_traces = xi_trace::samples_cloned_unsorted();

    let trace_output_path = match env::var("TRACE_OUTPUT") {
        Ok(output_path) => output_path,
        Err(_) => {
            println!("Environment variable TRACE_OUTPUT not set, defaulting to ./target/trace_output.trace");
            String::from("./target/trace_output.trace")
        }
    };

    let mut trace_file = match File::create(&trace_output_path) {
        Ok(f) => f,
        Err(_) => {
            println!(
                "Could not create trace output file at: {}.",
                &trace_output_path
            );
            return;
        }
    };

    if let Err(_) = chrome_trace_dump::serialize(&all_traces, &mut trace_file) {
        println!("Could not save trace file at: {}.", &trace_output_path);
    } else {
        println!("Saved trace file at: {}", &trace_output_path);
    }
}
