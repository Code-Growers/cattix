{ lib, ... }:

with lib;

let
  types = lib.types;

  targetType = types.submodule {
    options = {
      host = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Address used by cattix to connect to the host over SSH.";
      };
      port = mkOption {
        type = types.port;
        default = 22;
        description = "SSH port used to connect to the host.";
      };
      user = mkOption {
        type = types.str;
        default = "root";
        description = "SSH user used to connect to the host.";
      };
    };
  };

  metadataType = types.submodule {
    options = {
      environment = mkOption {
        type = types.str;
        default = "";
        description = "Environment in which the host operates.";
      };
      criticality = mkOption {
        type = types.str;
        default = "";
        description = "Operational criticality of the host or service.";
      };
      owner = mkOption {
        type = types.str;
        default = "";
        description = "Team or person responsible for the host or service.";
      };
    };
  };

  stringMatchType = types.submodule {
    options = {
      type = mkOption {
        type = types.enum [ "exact" "exactInsensitive" "contains" ];
        description = "How to compare the observed string with value.";
      };
      value = mkOption {
        type = types.str;
        description = "Expected string or substring.";
      };
    };
  };

  statusRangeType = types.submodule {
    options = {
      min = mkOption {
        type = types.ints.between 100 599;
        description = "Inclusive lowest healthy HTTP status code.";
      };
      max = mkOption {
        type = types.ints.between 100 599;
        description = "Inclusive highest healthy HTTP status code.";
      };
    };
  };

  jsonMatchType = types.submodule {
    options = {
      path = mkOption {
        type = types.str;
        description = "JSONPath expression selecting the value to compare.";
      };
      matcher = mkOption {
        type = stringMatchType;
        description = "Matcher applied to the selected JSON value.";
      };
    };
  };

  httpCheckType = types.submodule {
    options = {
      url = mkOption {
        type = types.str;
        description = "URL requested by the HTTP probe from the selected location.";
      };
      method = mkOption {
        type = types.enum [ "options" "get" "post" "put" "delete" "head" "trace" "connect" "patch" ];
        default = "get";
        description = "HTTP method used for the probe request.";
      };
      expectedStatus = mkOption {
        type = types.nullOr (types.ints.between 100 599);
        default = null;
        description = "Exact HTTP status code that marks the probe as healthy; defaults to 200 when no status range is configured.";
      };
      expectedStatusRange = mkOption {
        type = types.nullOr statusRangeType;
        default = null;
        description = "Inclusive HTTP status range that marks the probe as healthy. Mutually exclusive with expectedStatus.";
      };
      expectedHeaders = mkOption {
        type = types.attrsOf stringMatchType;
        default = {};
        description = "Response headers that must match; names are matched case-insensitively by HTTP.";
      };
      expectedBody = mkOption {
        type = types.nullOr stringMatchType;
        default = null;
        description = "Matcher applied to the UTF-8 response body.";
      };
      expectedJson = mkOption {
        type = types.nullOr jsonMatchType;
        default = null;
        description = "JSONPath matcher applied to an application/json response body.";
      };
      proxy = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional proxy URL used by the HTTP probe.";
      };
      tlsInsecure = mkOption {
        type = types.bool;
        default = false;
        description = "Accept invalid TLS certificates for this HTTP probe.";
      };
      timeoutMs = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Maximum duration of one HTTP-probe attempt in milliseconds.";
      };
    };
  };

  tcpCheckType = types.submodule {
    options = {
      addr = mkOption {
        type = types.str;
        description = "Socket address, including port, opened by the TCP probe from the selected location.";
      };
      timeoutMs = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Maximum duration of one TCP-probe attempt in milliseconds.";
      };
    };
  };

  grpcCheckType = types.submodule {
    options = {
      url = mkOption {
        type = types.str;
        description = "gRPC endpoint URL contacted by the health probe from the selected location.";
      };
      service = mkOption {
        type = types.str;
        description = "gRPC service name passed to the standard health-check endpoint.";
      };
      timeoutMs = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Maximum duration of one gRPC-probe attempt in milliseconds.";
      };
    };
  };

  commandCheckType = types.submodule {
    options = {
      program = mkOption {
        type = types.str;
        description = "Executable to run without shell parsing.";
      };
      args = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Arguments passed to the executable in order.";
      };
      expectedStatus = mkOption {
        type = types.int;
        default = 0;
        description = "Exit status that marks this command probe as healthy.";
      };
      expectedStdout = mkOption {
        type = types.nullOr stringMatchType;
        default = null;
        description = "Optional matcher applied to the command's standard output.";
      };
      expectedStderr = mkOption {
        type = types.nullOr stringMatchType;
        default = null;
        description = "Optional matcher applied to the command's standard error.";
      };
      timeoutMs = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "Maximum duration of one command-probe attempt in milliseconds.";
      };
    };
  };

  healthCheckType = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "Stable name used to identify the health check.";
      };
      command = mkOption {
        type = types.nullOr commandCheckType;
        default = null;
        description = "Structured command probe configuration.";
      };
      location = mkOption {
        type = types.nullOr (types.enum [ "controller" "target" ]);
        default = null;
        description = "Where to run the check. Commands default to target; network probes default to controller. Target-local network probes run through the copied cattix-probe-runner.";
      };
      http = mkOption {
        type = types.nullOr httpCheckType;
        default = null;
        description = "HTTP health probe executed from the selected location.";
      };
      tcp = mkOption {
        type = types.nullOr tcpCheckType;
        default = null;
        description = "TCP health probe executed from the selected location.";
      };
      grpc = mkOption {
        type = types.nullOr grpcCheckType;
        default = null;
        description = "gRPC health probe executed from the selected location.";
      };
      timeout = mkOption {
        type = types.ints.positive;
        default = 300;
        description = "Overall deadline in seconds for this check to pass.";
      };
      interval = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Seconds to wait between failed probe attempts.";
      };
    };
  };

in
{
  options.cattix = {
    group = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Rollout group this host belongs to.";
    };
    order = mkOption {
      type = types.ints.unsigned;
      default = 0;
      description = "Host order within its rollout group; lower values deploy first.";
    };
    target = mkOption {
      type = targetType;
      default = {};
      description = "Connection details for probing and managing the host.";
    };
    metadata = mkOption {
      type = metadataType;
      default = {};
      description = "Human and inventory metadata owned by the Nix configuration.";
    };
    healthChecks = mkOption {
      type = types.listOf healthCheckType;
      default = [];
      description = "Checks that must pass after activating a new system generation.";
    };
  };
}
