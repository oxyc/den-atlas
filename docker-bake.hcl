variable "ATLAS_IMAGE" {
  default = "ghcr.io/oxyc/den-atlas"
}

variable "EMBED_IMAGE" {
  default = "ghcr.io/oxyc/den-atlas-embed"
}

variable "INTEGRATED_IMAGE" {
  default = "ghcr.io/oxyc/den-atlas-integrated"
}

variable "TAG" {
  default = "local"
}

variable "DEN_VERSION" {
  default = "0.0.0"
}

variable "GIT_REVISION" {
  default = "unknown"
}

target "common" {
  context    = "."
  dockerfile = "Dockerfile"
  platforms = ["linux/amd64"]
  args = {
    DEN_VERSION = DEN_VERSION
    GIT_REVISION = GIT_REVISION
  }
}

target "atlas" {
  inherits = ["common"]
  target   = "runtime"
  tags     = ["${ATLAS_IMAGE}:${TAG}"]
}

# The HTTP-compatible corpus builder and the opt-in in-process Atlas image share the same engine, model
# and builder layers. Keeping them in one bake invocation makes that ownership explicit and avoids building
# the 555 MB model twice in CI.
target "compat" {
  inherits = ["common"]
  target   = "den-embed"
  tags     = ["${EMBED_IMAGE}:${TAG}"]
}

target "integrated" {
  inherits = ["common"]
  target   = "den-atlas-embedded"
  tags     = ["${INTEGRATED_IMAGE}:${TAG}"]
}

group "release" {
  targets = ["atlas", "compat", "integrated"]
}

group "optional" {
  targets = ["compat", "integrated"]
}
