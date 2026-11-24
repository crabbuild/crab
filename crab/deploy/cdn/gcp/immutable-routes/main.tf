variable "physical_repository_prefix" {
  type = string
}

variable "protected_view" {
  type    = bool
  default = false
}

locals {
  repository = "/${replace(var.physical_repository_prefix, ".", "\\.")}/"
  git_kinds  = ["capsules", "checkpoints", "catalogs", "pack-layers", "history"]
  xet_kinds  = ["xorbs", "shards", "capsule-chunk-pages"]
  paths = merge({
    git = {
      prefix     = "${local.repository}v2/(${join("|", local.git_kinds)}|history/source-roots)/"
      fanout     = "[a-f0-9]{2}"
      characters = 2
      positions  = [0, 1]
    }
    lfs = {
      prefix     = "${local.repository}lfs/objects/"
      fanout     = "[a-f0-9]{2}/[a-f0-9]{2}"
      characters = 5
      positions  = [0, 1, 3, 4]
    }
    }, var.protected_view ? {
    xet = {
      prefix     = "${local.repository}\\.crab/(${join("|", local.xet_kinds)})/"
      fanout     = "[a-f0-9]{2}"
      characters = 2
      positions  = [0, 1]
    }
  } : {})
  hex_digits = split("", "0123456789abcdef")
  wildcards  = "....."
  routes = {
    for name, path in local.paths : name => {
      shape = "^${path.prefix}${path.fanout}/[a-f0-9]{64}$"
      # A prior shape match fixes the positions of separators and digest bytes.
      # RE2 lacks backreferences, so enumerate each matching hex digit once.
      digits = [
        for offset, position in path.positions : "^${path.prefix}(${join("|", [
          for digit in local.hex_digits :
          "${substr(local.wildcards, 0, position)}${digit}${substr(local.wildcards, 0, path.characters - position - 1)}/${substr(local.wildcards, 0, offset)}${digit}"
        ])})"
      ]
    }
  }
  route_checks = flatten([
    for route in values(local.routes) : [
      for digit in route.digits :
      format("request.path.matches(R\"%s\") && !request.path.matches(R\"%s\")", route.shape, digit)
    ]
  ])
  fill_prefixes = concat(
    [for kind in local.git_kinds : "v2/${kind}"],
    ["lfs/objects"],
    var.protected_view ? [for kind in local.xet_kinds : ".crab/${kind}"] : []
  )
}

output "routes" {
  value = local.routes
}

output "fanout_denials" {
  value = local.route_checks
}

output "fill_prefixes" {
  value = local.fill_prefixes
}
