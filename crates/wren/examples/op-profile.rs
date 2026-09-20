//! What a benchmark actually asks the interpreter to do, opcode by opcode.
//!
//!     cargo run -p wren --release --features profile --example op-profile
//!     cargo run -p wren --release --features profile --example op-profile method_call
//!
//! **A histogram cannot be misattributed.** A sampling profiler on this
//! project has twice named the wrong thing -- it reports the frame the sample
//! landed in, which after an instruction-fetch stall is whatever retires next
//! rather than whatever stalled. This counts what the bytecode said, which is
//! not a matter of opinion. It is a host tool for the same reason the heap
//! profiler is: what it measures is counts, and a count does not care which
//! machine ran it.
//!
//! What it is *for* is choosing which interpreter arm to work on. An arm worth
//! optimising is one that runs often here; how much each one then costs is a
//! question for the board and the chip's performance counter. See
//! `doc/wren-rs/profiling.md`.

use wren::bytecode::Op;
use wren::Vm;

fn main() {
    let only: Option<String> = std::env::args().nth(1);

    let mut files: Vec<_> = std::fs::read_dir("benchmarks/wren")
        .expect("benchmarks/wren -- run this from the repository root")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|suffix| suffix == "wren"))
        .filter(|path| match &only {
            Some(name) => path.file_stem().is_some_and(|stem| stem == name.as_str()),
            None => true,
        })
        .collect();
    files.sort();

    if files.is_empty() {
        eprintln!("no benchmark matched {only:?}");
        std::process::exit(1);
    }

    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).expect("benchmark source");

        let mut vm = Vm::new();
        if let Err(error) = vm.interpret(&source) {
            eprintln!("{name}: {}", error.message());
            continue;
        }

        // What every compiled function costs, before it runs anything.
        let (mut code, mut lines, mut constants, mut lookup) = (0, 0, 0, 0);
        let mut functions = 0;
        for index in 0..vm.heap.function_count() {
            if let Some(chunk) = vm.heap.function_chunk(index) {
                let (c, l, k, u) = chunk.footprint();
                code += c;
                lines += l;
                constants += k;
                lookup += u;
                functions += 1;
            }
        }
        println!();
        println!("what the compiled program costs ({functions} functions)");
        println!("  code          {code:>8} B");
        println!("  line table    {lines:>8} B   ({:.1}x the code)", lines as f64 / code.max(1) as f64);
        println!("  constants     {constants:>8} B");
        println!("  constant index{lookup:>8} B   (compile-time only, still held)");
        println!("  total         {:>8} B", code + lines + constants + lookup);

        // **Static instruction lengths**, which is what a uniform encoding
        // would have to pay for. Executed frequency is a different question
        // and is the histogram above.
        let mut by_length = std::collections::BTreeMap::new();
        let mut instructions = 0usize;
        // Against the bytes actually walked, not `code.capacity()` above --
        // a `Vec` holds more than it uses and comparing with that flatters
        // every alternative encoding.
        let mut walked = 0usize;
        for index in 0..vm.heap.function_count() {
            let Some(chunk) = vm.heap.function_chunk(index) else {
                continue;
            };
            let mut at = 0;
            while at < chunk.code.len() {
                let Some(len) = wren::bytecode::Chunk::instruction_units(&chunk.code, at) else {
                    break;
                };
                *by_length.entry(len).or_insert(0usize) += 1;
                instructions += 1;
                walked += len;
                at += len;
            }
        }
        // **An instruction is measured in u16 units; a byte figure is twice
        // it.** Mixing the two is how this table came to compare unit counts
        // against byte counts and report a uniform width as costing twice
        // what it does.
        const BYTES_PER_UNIT: usize = core::mem::size_of::<u16>();
        let actual = walked * BYTES_PER_UNIT;
        println!();
        println!(
            "instruction lengths, statically ({instructions} instructions, \
             {walked} units = {actual} B)"
        );
        for (len, count) in by_length.iter() {
            println!(
                "  {len} unit{:<3} {count:>6}  {:>5.1}%   {:>6} B",
                if *len == 1 { "" } else { "s" },
                *count as f64 * 100.0 / instructions.max(1) as f64,
                len * count * BYTES_PER_UNIT
            );
        }
        // What a uniform width would cost. Six bytes is the width that holds
        // every instruction here; four holds all but the three-unit ones.
        let widest = by_length.keys().copied().max().unwrap_or(1);
        for (name, padded) in [
            ("every instruction 4 B", instructions * 4),
            ("every instruction 6 B", instructions * 6),
        ] {
            println!(
                "  {name:<28} {padded:>6} B ({:+.0}%){}",
                (padded as f64 - actual as f64) * 100.0 / actual.max(1) as f64,
                match widest * BYTES_PER_UNIT > 4 && name.contains('4') {
                    true => "  -- too narrow for the widest instruction here",
                    false => "",
                }
            );
        }

        println!();
        match vm.heap.block_census() {
            Some((addressed, held, given_back)) => println!(
                "slot blocks: {held} held of {addressed} addressed, {given_back} B given back"
            ),
            None => println!("slot blocks: flat tables, nothing to give back"),
        }

        let census = vm.heap.slot_census();
        let held: usize = census.iter().map(|(_, slots, _, size)| slots * size).sum();
        let live: usize = census
            .iter()
            .map(|(_, slots, free, size)| (slots - free) * size)
            .sum();
        println!();
        println!("slot tables at the end of the run");
        println!("{:<10} {:>8} {:>8} {:>10} {:>10}", "type", "slots", "live", "held B", "live B");
        println!("{}", "-".repeat(50));
        for (name, slots, free, size) in census.iter() {
            if *slots == 0 {
                continue;
            }
            println!(
                "{name:<10} {slots:>8} {:>8} {:>10} {:>10}",
                slots - free,
                slots * size,
                (slots - free) * size
            );
        }
        println!(
            "{:<10} {:>8} {:>8} {held:>10} {live:>10}   ({:.1}% of the slots held are free)",
            "total", "", "",
            (held - live) as f64 * 100.0 / held.max(1) as f64
        );

        let total: u64 = vm.op_counts.iter().sum();
        println!();
        println!("{name}: {total} instructions executed");
        println!();
        println!("{:<20} {:>14} {:>8}", "opcode", "count", "share");
        println!("{}", "-".repeat(56));

        // Sorted by count, because the question is always "what runs most".
        let mut rows: Vec<(u8, u64)> = vm
            .op_counts
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(byte, &count)| (byte as u8, count))
            .collect();
        rows.sort_by_key(|&(_, count)| core::cmp::Reverse(count));

        let mut running = 0u64;
        for (byte, count) in rows {
            running += count;
            let label = match Op::from_byte(byte) {
                Some(op) => format!("{op:?}"),
                None => format!("<{byte}>"),
            };
            println!(
                "{label:<20} {count:>14} {:>7.2}%  {:>6.1}% cumulative",
                count as f64 * 100.0 / total as f64,
                running as f64 * 100.0 / total as f64,
            );
        }

        // **Adjacent pairs, which is what a peephole pass can act on.** One
        // opcode costs about thirty-one machine instructions and nearly all of
        // it is dispatch, so a pair fused into a single instruction saves that
        // thirty-one however cheap the two halves are.
        let mut pairs: Vec<(usize, u64)> = vm
            .op_pairs
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(index, &count)| (index, count))
            .collect();
        pairs.sort_by_key(|&(_, count)| core::cmp::Reverse(count));

        let paired: u64 = pairs.iter().map(|&(_, count)| count).sum();
        println!();
        println!("{:<40} {:>14} {:>8}", "adjacent pair", "count", "share");
        println!("{}", "-".repeat(64));
        // **What a method cache would have to answer.** Not how many lookups
        // there are -- how few distinct questions they ask, and whether each
        // call site asks the same one every time.
        let asked: u64 = vm.lookups.values().sum();
        let sites_total: u64 = vm.call_sites.values().map(|(_, count)| count).sum();
        let monomorphic: u64 = vm
            .call_sites
            .values()
            .filter(|(classes, _)| classes.len() == 1)
            .map(|(_, count)| count)
            .sum();
        println!();
        println!("method lookups");
        println!(
            "  {asked} lookups asking {} distinct (class, symbol) pairs",
            vm.lookups.len()
        );
        println!(
            "  {} call sites, {} of them monomorphic -- {:.1}% of lookups",
            vm.call_sites.len(),
            vm.call_sites.values().filter(|(c, _)| c.len() == 1).count(),
            monomorphic as f64 * 100.0 / sites_total.max(1) as f64
        );

        // **The symbol has to fit in a byte for a one-unit Call to be
        // possible**, and the count of distinct symbols does not answer that:
        // a symbol is an index into the VM's whole method-name table, which
        // holds every core signature whether this program calls it or not. So
        // report the largest index actually reached, not how many there are.
        let widest = vm.lookups.keys().map(|&(_, symbol)| symbol).max().unwrap_or(0);
        println!(
            "  widest method symbol reached: {widest} -- {} in one byte",
            match widest < 256 {
                true => "fits",
                false => "DOES NOT fit",
            }
        );

        // **Where the instructions were written, not only what they were.**
        // The opcode histogram says what ran; this says which line of the
        // program asked for it, which is what turns a profile into a thing a
        // programmer can act on. The right-hand column is the mix, because a
        // line that is slow for running many cheap instructions wants a
        // different fix from one that is slow for running few expensive ones.
        let mut per_line: std::collections::BTreeMap<u16, u64> = Default::default();
        for (&(line, _), &count) in &vm.line_ops {
            *per_line.entry(line).or_insert(0) += count;
        }
        let executed: u64 = per_line.values().sum();
        let mut ranked: Vec<(u16, u64)> = per_line.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));

        println!();
        println!(
            "{:>5} {:>12} {:>7} {:>7}  {:<30} {}",
            "line", "ops", "share", "cumul", "source", "what it runs"
        );
        println!("{}", "-".repeat(110));
        let text: Vec<&str> = source.lines().collect();
        let mut running = 0u64;
        for (line, count) in ranked.iter().take(12) {
            running += count;
            // The mix on this line, biggest first.
            let mut mix: Vec<(u8, u64)> = vm
                .line_ops
                .iter()
                .filter(|(&(at, _), _)| at == *line)
                .map(|(&(_, op), &n)| (op, n))
                .collect();
            mix.sort_by(|a, b| b.1.cmp(&a.1));
            let mix = mix
                .iter()
                .take(4)
                .map(|(op, n)| match Op::from_byte(*op) {
                    Some(op) => format!("{op:?} {n}"),
                    None => format!("<{op}> {n}"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let written = text
                .get(*line as usize - 1)
                .map(|line| line.trim())
                .unwrap_or("");
            let written: String = written.chars().take(30).collect();
            println!(
                "{line:>5} {count:>12} {:>6.2}% {:>6.1}%  {written:<30} {mix}",
                *count as f64 * 100.0 / executed.max(1) as f64,
                running as f64 * 100.0 / executed.max(1) as f64
            );
        }

        // Who the lookups are actually for, by name. This is the table that
        // says which core methods are worth specialising: a handful of them
        // carry almost all of the dispatch in every benchmark.
        let mut by_pair: Vec<((u32, u32), u64)> =
            vm.lookups.iter().map(|(&key, &count)| (key, count)).collect();
        by_pair.sort_by(|a, b| b.1.cmp(&a.1));
        println!();
        println!(
            "{:<34} {:>12} {:>8} {:>8}",
            "receiver class and method", "lookups", "share", "cumul"
        );
        println!("{}", "-".repeat(66));
        let mut running = 0u64;
        for ((class, symbol), count) in by_pair {
            running += count;
            let class_name = match vm.heap.class(wren::ObjectId::new(class)) {
                Some(object) => match vm.heap.string(object.name) {
                    Some(text) => text.as_str().unwrap_or("?").to_string(),
                    None => format!("class#{class}"),
                },
                None => format!("class#{class}"),
            };
            let method = vm
                .method_names
                .name(symbol as usize)
                .unwrap_or("?")
                .to_string();
            println!(
                "{:<34} {count:>12} {:>7.2}% {:>7.1}%",
                format!("{class_name}.{method}"),
                count as f64 * 100.0 / asked.max(1) as f64,
                running as f64 * 100.0 / asked.max(1) as f64
            );
        }

        for (index, count) in pairs.into_iter().take(10) {
            let name = |byte: u8| match Op::from_byte(byte) {
                Some(op) => format!("{op:?}"),
                None => format!("<{byte}>"),
            };
            let label = format!("{} -> {}", name((index / 256) as u8), name((index % 256) as u8));
            println!(
                "{label:<40} {count:>14} {:>7.2}%",
                count as f64 * 100.0 / paired as f64
            );
        }
    }
}
