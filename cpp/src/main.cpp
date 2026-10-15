// The command line, matching the other ports where it overlaps.
//
// Exit codes: 0 identical, 1 differences found, 2 error.
#include "csvdiff.hpp"

#include <fstream>
#include <iostream>
#include <sstream>

namespace {

const char* kUsage = R"(csvdiff - composite-key CSV comparison, byte-level

usage:
  csvdiff compare A B -k COLS [options]

options:
  -k, --key COLS        comma-separated key column(s), required
  -c, --compare COLS    columns to compare (default: all common non-key columns)
  -i, --ignore COLS     columns to skip
      --trim            strip whitespace before comparing
      --ignore-case
      --empty-is-null   treat an empty string and an absent value as equal
      --tolerance N     absolute numeric tolerance
      --max-rows N      rows embedded per section (default 50000)
      --delimiter D     force the delimiter (default: sniff it)
      --json PATH       write the JSON summary here
      --engine E        turbo (the only one; accepted so the flag is portable)

exit codes: 0 identical, 1 differences found, 2 error
)";

std::vector<std::string> split(const std::string& s) {
    std::vector<std::string> out;
    std::stringstream ss(s);
    std::string item;
    while (std::getline(ss, item, ',')) {
        if (!item.empty()) out.push_back(item);
    }
    return out;
}

}  // namespace

int main(int argc, char** argv) {
    std::vector<std::string> args(argv + 1, argv + argc);
    if (args.empty() || args[0] == "-h" || args[0] == "--help") {
        std::cout << kUsage;
        return args.empty() ? 2 : 0;
    }
    if (args[0] != "compare") {
        std::cerr << "error: unknown command: " << args[0] << "\n";
        return 2;
    }

    csvdiff::Options opt;
    std::string a_path, b_path, json_path;
    std::vector<std::string> positional;

    try {
        for (std::size_t i = 1; i < args.size(); ++i) {
            const std::string& f = args[i];
            const auto next = [&]() -> std::string {
                if (i + 1 >= args.size()) throw csvdiff::Error(f + " needs a value");
                return args[++i];
            };
            if (f == "-k" || f == "--key") opt.key = split(next());
            else if (f == "-c" || f == "--compare") opt.compare = split(next());
            else if (f == "-i" || f == "--ignore") opt.ignore = split(next());
            else if (f == "--trim") opt.trim = true;
            else if (f == "--ignore-case") opt.ignore_case = true;
            else if (f == "--empty-is-null") opt.empty_is_null = true;
            else if (f == "--tolerance") opt.tolerance = std::stod(next());
            else if (f == "--max-rows") opt.max_rows = std::stoul(next());
            else if (f == "--delimiter") opt.delimiter = next().at(0);
            else if (f == "--threads") opt.threads = static_cast<unsigned>(std::stoul(next()));
            else if (f == "--json") json_path = next();
            else if (f == "--engine") next();  // accepted and ignored: there is one
            else if (f == "-o" || f == "--out") next();  // no HTML report in this port
            else if (!f.empty() && f[0] == '-') throw csvdiff::Error("unknown option: " + f);
            else positional.push_back(f);
        }
        if (positional.size() != 2) throw csvdiff::Error("compare needs exactly two files");
        a_path = positional[0];
        b_path = positional[1];
        if (opt.key.empty()) throw csvdiff::Error("--key is required");

        // Nothing but the summary line is printed unless --json was given, and
        // the summary is counts. Collecting the row samples then costs a pass
        // over B for output nobody asked for.
        opt.row_lists = !json_path.empty();

        // Left for the process's exit to reclaim rather than freed here. With
        // --json it holds the report's row samples -- 50,000 changed rows by
        // default, each a vector of strings -- and taking that apart one
        // allocation at a time was 45M instructions on a 400k pair, on a path
        // that returns straight to the OS. The Rust port forgets its result for
        // the same reason (#252).
        const csvdiff::Result& r = *new csvdiff::Result(csvdiff::compare(a_path, b_path, opt));

        if (!json_path.empty()) {
            // std::ios::binary so the Microsoft runtime does not turn every \n in
            // the payload into \r\n; the other ports write bytes.
            std::ofstream out(json_path, std::ios::binary);
            if (!out) throw csvdiff::Error("cannot write " + json_path);
            out << csvdiff::to_json(r, a_path, b_path, opt);
        }

        const csvdiff::Counts& c = r.counts;
        std::cout << "A " << c.a_rows << " rows | B " << c.b_rows << " rows | matched " << c.matched
                  << " (changed " << c.changed << ") | added " << c.added << " | removed "
                  << c.removed << " | dup keys A " << c.a_dup_keys << " B " << c.b_dup_keys
                  << " | turbo " << r.seconds << "s\n";
        return r.identical() ? 0 : 1;
    } catch (const csvdiff::Error& e) {
        std::cerr << "error: " << e.message << "\n";
        return 2;
    } catch (const std::exception& e) {
        std::cerr << "error: " << e.what() << "\n";
        return 2;
    }
}
