// Dumps a parquet column as text, so the reader can be checked against the CSV
// the file was made from before anything depends on it.
#include "../src/parquet.hpp"

#include <fcntl.h>
#include "../src/win32.hpp"   // O_BINARY: a no-op off Windows
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cstdio>
#include <cstring>
#include <string>

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: pq-dump FILE [column] [limit]\n");
        return 2;
    }
    const std::string path = argv[1];
    const int fd = ::open(path.c_str(), O_RDONLY | O_BINARY);
    struct stat st{};
    if (fd < 0 || ::fstat(fd, &st) != 0) {
        std::fprintf(stderr, "cannot read %s\n", path.c_str());
        return 2;
    }
    const std::size_t size = static_cast<std::size_t>(st.st_size);
    const char* data = static_cast<const char*>(::mmap(nullptr, size, PROT_READ, MAP_PRIVATE, fd, 0));
    if (data == MAP_FAILED) return 2;

    try {
        const auto meta = csvdiff::parquet::read_meta(data, size, path);
        if (argc == 2) {
            std::printf("%lld rows, %zu row groups, %zu columns\n",
                        static_cast<long long>(meta.rows), meta.row_groups, meta.names.size());
            for (std::size_t i = 0; i < meta.names.size(); ++i)
                std::printf("  %2zu %s\n", i, meta.names[i].c_str());
            return 0;
        }
        std::size_t which = meta.names.size();
        for (std::size_t i = 0; i < meta.names.size(); ++i)
            if (meta.names[i] == argv[2]) which = i;
        if (which == meta.names.size()) {
            std::fprintf(stderr, "no column named %s\n", argv[2]);
            return 2;
        }
        const long limit = argc > 3 ? std::atol(argv[3]) : 10;
        const auto col = csvdiff::parquet::read_column(data, size, which, path);
        std::fprintf(stderr, "%s: %s, %zu rows, %zu dictionary entries\n", argv[2],
                     col.dictionary ? "dictionary" : "plain", col.rows(), col.dict.size());
        const char* base = col.owned.empty() ? data : col.owned.data();
        for (std::size_t i = 0; i < col.rows() && (limit < 0 || static_cast<long>(i) < limit); ++i) {
            csvdiff::parquet::Slice s{};
            if (col.dictionary) {
                const std::int32_t d = col.index[i];
                if (d >= 0) s = col.dict[static_cast<std::size_t>(d)];
            } else {
                s = col.values[i];
            }
            if (s.null()) std::printf("\n");
            else std::printf("%.*s\n", static_cast<int>(s.length()), base + s.offset());
        }
    } catch (const std::exception& e) {
        std::fprintf(stderr, "error: %s\n", e.what());
        return 2;
    }
    return 0;
}
