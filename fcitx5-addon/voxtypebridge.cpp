// SPDX-License-Identifier: MIT

#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <string>
#include <string_view>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <fcitx-utils/capabilityflags.h>
#include <fcitx-utils/event.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/trackableobject.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/inputcontext.h>
#include <fcitx/instance.h>

namespace fcitx {
namespace {

constexpr std::size_t MaxDatagramSize = 64 * 1024;

std::string runtimeSocketPath() {
    const char *runtime = std::getenv("XDG_RUNTIME_DIR");
    if (!runtime || !*runtime) {
        return {};
    }
    return std::string(runtime) + "/voxtype/fcitx.sock";
}

std::string_view field(std::string_view message, std::size_t index) {
    std::size_t begin = 0;
    for (std::size_t current = 0; current < index; ++current) {
        const auto separator = message.find('\0', begin);
        if (separator == std::string_view::npos) {
            return {};
        }
        begin = separator + 1;
    }
    const auto end = message.find('\0', begin);
    return message.substr(begin, end == std::string_view::npos ? end : end - begin);
}

bool isSecure(InputContext *context) {
    return context->capabilityFlags().testAny(
        CapabilityFlags{CapabilityFlag::Password, CapabilityFlag::Sensitive});
}

std::string response(std::string_view status, std::string_view detail) {
    std::string value(status);
    value.push_back('\0');
    value.append(detail);
    return value;
}

} // namespace

class VoxTypeBridgeConfig final : public Configuration {
public:
    VoxTypeBridgeConfig()
        : settings(this, "Settings", "Open VoxType settings",
                   "voxtype-settings") {}

    const char *typeName() const override { return "VoxTypeBridgeConfig"; }

    ExternalOption settings;
};

class VoxTypeBridge final : public AddonInstance {
public:
    explicit VoxTypeBridge(Instance *instance) : instance_(instance) {
        openSocket();
    }

    ~VoxTypeBridge() override {
        ioEvent_.reset();
        if (socketFd_ >= 0) {
            close(socketFd_);
        }
        if (!socketPath_.empty()) {
            unlink(socketPath_.c_str());
        }
    }

    const Configuration *getConfig() const override { return &config_; }

    void setConfig(const RawConfig &config) override {
        config_.load(config, true);
    }

private:
    void openSocket() {
        socketPath_ = runtimeSocketPath();
        if (socketPath_.empty()) {
            FCITX_ERROR() << "VoxType bridge: XDG_RUNTIME_DIR is unavailable";
            return;
        }
        std::error_code error;
        std::filesystem::create_directories(
            std::filesystem::path(socketPath_).parent_path(), error);
        if (error) {
            FCITX_ERROR() << "VoxType bridge: could not create runtime directory: "
                          << error.message();
            return;
        }
        chmod(std::filesystem::path(socketPath_).parent_path().c_str(), 0700);

        socketFd_ = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if (socketFd_ < 0) {
            FCITX_ERROR() << "VoxType bridge: socket failed: "
                          << std::strerror(errno);
            return;
        }
        sockaddr_un address{};
        address.sun_family = AF_UNIX;
        if (socketPath_.size() >= sizeof(address.sun_path)) {
            FCITX_ERROR() << "VoxType bridge: socket path is too long";
            close(socketFd_);
            socketFd_ = -1;
            return;
        }
        std::memcpy(address.sun_path, socketPath_.c_str(), socketPath_.size() + 1);
        unlink(socketPath_.c_str());
        if (bind(socketFd_, reinterpret_cast<sockaddr *>(&address),
                 sizeof(address)) < 0) {
            FCITX_ERROR() << "VoxType bridge: bind failed: "
                          << std::strerror(errno);
            close(socketFd_);
            socketFd_ = -1;
            return;
        }
        chmod(socketPath_.c_str(), 0600);
        ioEvent_ = instance_->eventLoop().addIOEvent(
            socketFd_, {IOEventFlag::In, IOEventFlag::Err, IOEventFlag::Hup},
            [this](EventSourceIO *, int, IOEventFlags flags) {
                if (flags.test(IOEventFlag::In)) {
                    receive();
                }
                return true;
            });
        FCITX_INFO() << "VoxType input-context bridge ready";
    }

    void receive() {
        std::array<char, MaxDatagramSize> buffer{};
        sockaddr_un sender{};
        socklen_t senderLength = sizeof(sender);
        const auto size = recvfrom(socketFd_, buffer.data(), buffer.size(), 0,
                                   reinterpret_cast<sockaddr *>(&sender),
                                   &senderLength);
        if (size <= 0) {
            return;
        }
        const std::string_view message(buffer.data(), static_cast<std::size_t>(size));
        const auto command = field(message, 0);
        std::string response;
        if (command == "PING") {
            response = fcitx::response("OK", "ready");
        } else if (command == "ARM") {
            response = arm(field(message, 1));
        } else if (command == "COMMIT") {
            response = commit(field(message, 1), field(message, 2));
        } else if (command == "CANCEL") {
            response = cancel(field(message, 1));
        } else {
            response = fcitx::response("ERR", "unknown-command");
        }
        sendto(socketFd_, response.data(), response.size(), 0,
               reinterpret_cast<sockaddr *>(&sender), senderLength);
    }

    std::string arm(std::string_view session) {
        auto *context = instance_->lastFocusedInputContext();
        if (!context || !context->hasFocus()) {
            return response("ERR", "no-focused-context");
        }
        if (isSecure(context)) {
            return response("ERR", "secure-context");
        }
        armedContext_ = context->watch();
        armedSession_ = session;
        std::string response{"OK\0armed\0", 9};
        response += context->program();
        response.push_back('\0');
        response += context->frontendName();
        return response;
    }

    std::string commit(std::string_view session, std::string_view text) {
        auto *context = armedContext_.get();
        if (session != armedSession_) {
            return response("ERR", "session-mismatch");
        }
        if (!context || !context->hasFocus() ||
            context != instance_->lastFocusedInputContext()) {
            clear();
            return response("ERR", "focus-changed");
        }
        if (isSecure(context)) {
            clear();
            return response("ERR", "secure-context");
        }
        if (text.empty()) {
            return response("ERR", "empty-text");
        }
        const std::string pendingText(text);
        pendingCommit_ = instance_->eventLoop().addDeferEvent(
            [this, pendingText](EventSource *) {
                auto *pendingContext = armedContext_.get();
                if (pendingContext && pendingContext->hasFocus() &&
                    pendingContext == instance_->lastFocusedInputContext() &&
                    !isSecure(pendingContext)) {
                    pendingContext->commitString(pendingText);
                }
                armedContext_.unwatch();
                armedSession_.clear();
                return true;
            });
        pendingCommit_->setOneShot();
        return response("OK", "queued");
    }

    std::string cancel(std::string_view session) {
        if (!armedSession_.empty() && session != armedSession_) {
            return response("ERR", "session-mismatch");
        }
        clear();
        return response("OK", "cancelled");
    }

    void clear() {
        pendingCommit_.reset();
        armedContext_.unwatch();
        armedSession_.clear();
    }

    Instance *instance_;
    VoxTypeBridgeConfig config_;
    int socketFd_ = -1;
    std::string socketPath_;
    std::unique_ptr<EventSourceIO> ioEvent_;
    std::unique_ptr<EventSource> pendingCommit_;
    TrackableObjectReference<InputContext> armedContext_;
    std::string armedSession_;
};

class VoxTypeBridgeFactory final : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new VoxTypeBridge(manager->instance());
    }
};

} // namespace fcitx

#ifdef FCITX_ADDON_FACTORY_V2
FCITX_ADDON_FACTORY_V2(voxtypebridge, fcitx::VoxTypeBridgeFactory);
#else
FCITX_ADDON_FACTORY(fcitx::VoxTypeBridgeFactory);
#endif
