#include "policy.h"

#include <algorithm>
#include <cstdlib>
#include <deque>
#include <filesystem>
#include <optional>
#include <ranges>
#include <set>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <fmt/format.h>

#include "sys.h"
#include "usage.h"

namespace hx {
namespace {

namespace fs = std::filesystem;

bool holds_home(const fs::path& project) {
    std::error_code ec;
    const fs::path home = fs::canonical(env_or("HOME", ""), ec);
    if (ec || home.empty()) {
        return false;
    }
    return beneath(home, project);
}

bool within(const fs::path& path, const std::vector<fs::path>& roots) {
    return std::ranges::any_of(roots, [&](const fs::path& root) { return beneath(path, root); });
}

std::optional<fs::path> resolve_trusted(const fs::path& path, const std::vector<fs::path>& writable) {
    constexpr int kMaxLinks = 40;
    std::deque<fs::path> pending(path.begin(), path.end());
    fs::path resolved = "/";
    int links = 0;
    while (!pending.empty()) {
        const fs::path part = std::move(pending.front());
        pending.pop_front();
        if (part.empty() || part == "/" || part == ".") {
            continue;
        }
        if (part == "..") {
            resolved = resolved.parent_path();
            continue;
        }
        fs::path next = resolved / part;
        std::error_code ec;
        if (!fs::is_symlink(next, ec)) {
            resolved = std::move(next);
            continue;
        }
        const fs::path target = fs::read_symlink(next, ec);
        if (ec || within(resolved, writable) || ++links > kMaxLinks) {
            return std::nullopt;
        }
        if (target.is_absolute()) {
            resolved = "/";
        }
        pending.insert(pending.begin(), target.begin(), target.end());
    }
    return resolved;
}

void add_path_rules(std::vector<Rule>& rules, const std::vector<fs::path>& writable) {
    std::vector<fs::path> readable = writable;
    for (const Rule& rule : rules) {
        std::error_code ec;
        if (fs::path whole = fs::canonical(rule.path, ec); !ec) {
            readable.push_back(std::move(whole));
        }
    }
    std::set<fs::path> opened;
    const auto open = [&](const fs::path& dir) {
        const std::optional<fs::path> whole = resolve_trusted(dir, writable);
        std::error_code ec;
        if (!whole || !fs::is_directory(*whole, ec) || *whole == "/" || holds_home(*whole)) {
            return;
        }
        if (!within(*whole, readable)) {
            opened.insert(*whole);
        }
    };
    for (const auto part : std::views::split(env_or("PATH", ""), ':')) {
        if (const std::string_view entry(part.begin(), part.end()); entry.starts_with('/')) {
            open(entry);
        }
    }
    const std::set<fs::path> listed = opened;
    for (const fs::path& dir : listed) {
        std::error_code ec;
        for (fs::directory_iterator it(dir, ec), end; !ec && it != end; it.increment(ec)) {
            std::error_code link_ec;
            if (!it->is_symlink(link_ec)) {
                continue;
            }
            const std::optional<fs::path> target = resolve_trusted(it->path(), writable);
            if (!target || !fs::exists(*target, link_ec) || listed.contains(target->parent_path())) {
                continue;
            }
            const fs::path home = target->parent_path();
            open(home);
            if (home.filename() == "bin") {
                for (const char* sibling : {"lib", "lib64", "libexec"}) {
                    open(home.parent_path() / sibling);
                }
            }
        }
    }
    for (const fs::path& dir : opened) {
        rules.push_back({.path = dir.string(), .access = Access::Read, .required = false});
    }
}

}  // namespace

WallPolicy::WallPolicy(fs::path project, std::vector<std::string> read_paths, std::vector<std::string> write_paths,
                       bool unix_sockets)
    : project_(std::move(project)),
      read_paths_(std::move(read_paths)),
      write_paths_(std::move(write_paths)),
      unix_sockets_(unix_sockets) {}

std::vector<fs::path> WallPolicy::writable_roots() const {
    std::vector<fs::path> roots{project_};
    for (const std::string& path : write_paths_) {
        std::error_code ec;
        if (fs::path whole = fs::canonical(path, ec); !ec) {
            roots.push_back(std::move(whole));
        }
    }
    return roots;
}

void WallPolicy::check() const {
    if (project_ == "/" || holds_home(project_)) {
        throw UsageError(
            fmt::format("refusing to treat {} as a project: the sandbox would leave your home directory open. "
                        "Run hx inside a project directory.",
                        project_.string()));
    }
    for (const auto& [flag, paths] : {std::pair{"--ro", &read_paths_}, std::pair{"--rw", &write_paths_}}) {
        for (const std::string& path : *paths) {
            std::error_code ec;
            const fs::path whole = fs::canonical(path, ec);
            if (!ec && (whole == "/" || holds_home(whole))) {
                throw UsageError(
                    fmt::format("refusing {} {}: it would open your whole home directory. Name the "
                                "directories inside it that the task needs, or pass --no-sandbox.",
                                flag, path));
            }
        }
    }
}

std::vector<Rule> WallPolicy::system_rules() const {
    std::vector<Rule> rules;
    for (const char* path : {
             "/usr",
             "/bin",
             "/sbin",
             "/lib",
             "/lib32",
             "/lib64",
             "/libx32",
             "/etc",
             "/opt",
             "/proc",
             "/sys",
             "/nix/store",
             "/run/systemd/resolve",
         }) {
        rules.push_back({.path = path, .access = Access::Read, .required = false});
    }
    for (const char* path : {"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"}) {
        rules.push_back({.path = path, .access = Access::Device, .required = false});
    }
    const std::string home = env_or("HOME", "");
    if (home.starts_with('/')) {
        rules.push_back({.path = home + "/.gitconfig", .access = Access::Read, .required = false});
    }
    std::string config = env_or("XDG_CONFIG_HOME", "");
    if (!config.starts_with('/') && home.starts_with('/')) {
        config = home + "/.config";
    }
    if (config.starts_with('/')) {
        for (const char* name : {"config", "ignore", "attributes"}) {
            rules.push_back(
                {.path = fmt::format("{}/git/{}", config, name), .access = Access::Read, .required = false});
        }
    }
    for (const char* name : {
             "SSL_CERT_FILE",
             "SSL_CERT_DIR",
             "CURL_CA_BUNDLE",
             "REQUESTS_CA_BUNDLE",
             "NODE_EXTRA_CA_CERTS",
             "GIT_SSL_CAINFO",
             "CARGO_HTTP_CAINFO",
             "AWS_CA_BUNDLE",
             "NIX_SSL_CERT_FILE",
         }) {
        if (std::string path = env_or(name, ""); path.starts_with('/')) {
            rules.push_back({.path = std::move(path), .access = Access::Read, .required = false});
        }
    }
    const std::vector<fs::path> writable = writable_roots();
    std::erase_if(rules, [&](const Rule& rule) {
        const std::optional<fs::path> whole = resolve_trusted(rule.path, writable);
        return !whole || within(*whole, writable);
    });
    add_path_rules(rules, writable);
    return rules;
}

std::vector<Rule> WallPolicy::user_rules() const {
    std::vector<Rule> rules;
    rules.reserve(read_paths_.size() + write_paths_.size());
    for (const std::string& path : read_paths_) {
        rules.push_back({.path = path, .access = Access::Read, .required = true});
    }
    for (const std::string& path : write_paths_) {
        rules.push_back({.path = path, .access = Access::Write, .required = true});
    }
    return rules;
}

std::vector<Rule> WallPolicy::settled_user_rules() const {
    const std::vector<fs::path> writable = writable_roots();
    std::vector<Rule> settled;
    for (Rule& rule : user_rules()) {
        std::error_code ec;
        const fs::path whole = fs::canonical(rule.path, ec);
        if (ec) {
            settled.push_back(std::move(rule));
        } else if (std::ranges::none_of(writable, [&](const fs::path& root) {
                       return (whole != root || root == project_) && beneath(whole, root);
                   })) {
            rule.path = whole.string();
            settled.push_back(std::move(rule));
        }
    }
    return settled;
}

std::vector<Rule> WallPolicy::agent_rules(const fs::path& tmp) const {
    std::vector<Rule> rules = system_rules();
    rules.push_back({.path = project_.string(), .access = Access::Write, .required = true});
    rules.push_back({.path = tmp.string(), .access = Access::Write, .required = true});
    const std::vector<Rule> user = user_rules();
    rules.insert(rules.end(), user.begin(), user.end());
    return rules;
}

Sandbox WallPolicy::confine(const fs::path& tmp) const {
    check();
    try {
        return Sandbox::confine(agent_rules(tmp), unix_sockets_);
    } catch (const SandboxError& error) {
        throw UsageError(fmt::format("no sandbox: {}. Pass --no-sandbox to run without one.", error.what()));
    }
}

JudgeWalls WallPolicy::judge_walls(const fs::path& tmp) const {
    return {
        .system = system_rules(),
        .user = settled_user_rules(),
        .agent = agent_rules(tmp),
        .unix_sockets = unix_sockets_,
    };
}

std::string WallPolicy::unwritable_path() const {
    constexpr std::string_view kShellDefault = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    const std::vector<fs::path> writable = writable_roots();
    const char* set = std::getenv("PATH");  // NOLINT(concurrency-mt-unsafe)
    std::string kept;
    for (const auto part : std::views::split(set != nullptr ? std::string_view(set) : kShellDefault, ':')) {
        const std::string_view entry(part.begin(), part.end());
        const std::optional<fs::path> whole = entry.starts_with('/') ? resolve_trusted(entry, writable) : std::nullopt;
        std::error_code ec;
        if (whole && !within(*whole, writable) && fs::is_directory(entry, ec)) {
            kept.append(kept.empty() ? "" : ":").append(entry);
        }
    }
    if (kept.empty()) {
        throw UsageError(
            "--judge: PATH has no directory the model cannot write, so the judge has nowhere to find its programs");
    }
    return kept;
}

}  // namespace hx
