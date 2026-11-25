window.onload = function () {
  window.ui = SwaggerUIBundle({
    url: "../openapi.json",
    dom_id: "#swagger-ui",
    deepLinking: true,
    filter: true,
    validatorUrl: null,
    withCredentials: true,
    presets: [SwaggerUIBundle.presets.apis, SwaggerUIStandalonePreset],
    plugins: [SwaggerUIBundle.plugins.DownloadUrl],
    layout: "StandaloneLayout",
    requestInterceptor: function (request) {
      const target = new URL(request.url, window.location.href);
      const method = (request.method || "GET").toUpperCase();
      if (target.origin === window.location.origin &&
          ["POST", "PUT", "PATCH", "DELETE"].includes(method)) {
        const csrf = document.cookie.split(";").map(value => value.trim())
          .find(value => value.startsWith("csrf_token="));
        if (csrf) {
          request.headers["X-CSRF-Token"] = decodeURIComponent(csrf.slice("csrf_token=".length));
        }
      }
      return request;
    }
  });
};
