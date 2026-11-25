/*!
 * LitePan 边车插件前端 SDK
 *
 * 用法(插件自己的页面里):
 *   <script src="/plugins/sdk.js"></script>
 *   <script>
 *     LitePanPlugin.ready(async (sdk) => {
 *       const stats = await sdk.call('data.stats', {});
 *       document.body.textContent = JSON.stringify(stats);
 *     });
 *   </script>
 *
 * 页面运行在 sandbox iframe 里(没有 allow-same-origin),不能直接访问父页面 DOM,
 * 也不能直接 fetch 宿主接口。所有能力都通过 postMessage 转给宿主,
 * 由宿主按 manifest.permissions 鉴权后调用 /api/plugins/<id>/bridge。
 */
(function () {
  "use strict";

  var PARENT = window.parent !== window ? window.parent : null;
  var pending = new Map();
  var seq = 0;
  var readyQueue = [];
  var isReady = false;
  var sdk = null;

  function pluginId() {
    // /api/plugins/<id>/ui/... → <id>
    var m = /\/api\/plugins\/([^/]+)\/ui(?:\/|$)/.exec(location.pathname);
    return m ? decodeURIComponent(m[1]) : "";
  }

  function post(message) {
    if (!PARENT) return;
    message.__litepan_plugin__ = true;
    message.plugin = pluginId();
    PARENT.postMessage(message, "*");
  }

  function call(capability, params) {
    return new Promise(function (resolve, reject) {
      if (!PARENT) {
        reject(new Error("插件页面不在宿主 iframe 中运行,无法调用容器能力"));
        return;
      }
      var id = ++seq;
      var timer = setTimeout(function () {
        if (pending.has(id)) {
          pending.delete(id);
          reject(new Error("调用超时:" + capability));
        }
      }, 60000);
      pending.set(id, { resolve: resolve, reject: reject, timer: timer });
      post({ type: "call", id: id, capability: capability, params: params || {} });
    });
  }

  function handleReply(data) {
    if (!data || data.__litepan_plugin_reply__ !== true) return;
    var entry = pending.get(data.id);
    if (!entry) return;
    pending.delete(data.id);
    clearTimeout(entry.timer);
    if (data.ok) entry.resolve(data.result);
    else {
      var err = new Error(data.error || "调用失败");
      err.permission = !!data.permission;
      entry.reject(err);
    }
  }

  function buildSdk() {
    return {
      id: pluginId(),
      call: call,
      log: function (message, level) {
        return call("log", { message: String(message), level: level || "info" });
      },
      config: {
        get: function () { return call("config.get", {}); },
        set: function (patch) { return call("config.set", { config: patch || {} }); }
      },
      data: {
        search: function (q, extra) {
          var payload = Object.assign({}, extra || {});
          if (q !== undefined && q !== null && q !== "") payload.q = q;
          return call("data.search", { payload: payload });
        },
        stats: function () { return call("data.stats", {}); }
      },
      storage: {
        get: function (key, fallback) {
          return call("storage.get", { key: key, default: fallback });
        },
        set: function (key, value) { return call("storage.set", { key: key, value: value }); },
        delete: function (key) { return call("storage.delete", { key: key }); },
        keys: function () { return call("storage.keys", {}); }
      },
      events: {
        emit: function (name, payload) {
          return call("events.emit", { name: name, payload: payload || {} });
        }
      },
      settings: {
        get: function () { return call("settings.get", {}); }
      },
      jobs: {
        list: function (limit) { return call("jobs.list", { limit: limit || 20 }); },
        get: function (id) { return call("jobs.get", { job_id: id }); }
      }
    };
  }

  window.addEventListener("message", function (event) {
    if (PARENT && event.source !== PARENT) return;
    var data = event.data;
    if (!data || typeof data !== "object") return;
    if (data.__litepan_host_ready__ === true) {
      if (!isReady) {
        isReady = true;
        sdk = buildSdk();
        var queue = readyQueue.slice();
        readyQueue.length = 0;
        queue.forEach(function (cb) { safely(cb, sdk); });
      }
      return;
    }
    handleReply(data);
  });

  function safely(cb, arg) {
    try {
      var out = cb(arg);
      if (out && typeof out.then === "function") {
        out.then(null, function (err) {
          console.error("[LitePanPlugin]", err);
        });
      }
    } catch (err) {
      console.error("[LitePanPlugin]", err);
    }
  }

  var LitePanPlugin = {
    get id() { return pluginId(); },
    call: call,
    ready: function (cb) {
      if (isReady) safely(cb, sdk || buildSdk());
      else readyQueue.push(cb);
    }
  };

  window.LitePanPlugin = LitePanPlugin;

  // 页面加载完成后主动告诉宿主"我准备好了",宿主会回 host_ready
  function announce() {
    post({ type: "ready" });
  }
  if (document.readyState === "complete" || document.readyState === "interactive") {
    setTimeout(announce, 0);
  } else {
    document.addEventListener("DOMContentLoaded", announce);
  }
  window.addEventListener("load", announce);
})();
