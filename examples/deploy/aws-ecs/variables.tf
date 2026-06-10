variable "aws_region" {
  description = "AWS region to deploy into."
  type        = string
  default     = "us-east-1"
}

variable "name_prefix" {
  description = "Prefix applied to all created resource names."
  type        = string
  default     = "powdb"
}

variable "powdb_image" {
  description = "Container image for powdb-server."
  type        = string
  default     = "ghcr.io/zvn-dev/powdb:v0.4.7"
}

variable "powdb_port" {
  description = "TCP port the PowDB wire protocol listens on."
  type        = number
  default     = 5433
}

variable "task_cpu" {
  description = "Fargate task CPU units (256 = 0.25 vCPU)."
  type        = number
  default     = 512
}

variable "task_memory" {
  description = "Fargate task memory in MiB."
  type        = number
  default     = 1024
}

variable "powdb_password_secret_arn" {
  description = "ARN of a Secrets Manager secret holding POWDB_PASSWORD."
  type        = string
}

variable "require_tls" {
  description = "Set POWDB_REQUIRE_TLS so the server refuses to start if a password is set without TLS configured."
  type        = bool
  default     = true
}

variable "query_memory_limit_bytes" {
  description = "Per-query memory budget passed via POWDB_QUERY_MEMORY_LIMIT."
  type        = number
  default     = 268435456 # 256 MiB
}

variable "allowed_cidr_blocks" {
  description = "CIDR blocks allowed to reach the PowDB TCP port. Defaults to private RFC1918; override for public access."
  type        = list(string)
  default     = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
}

variable "assign_public_ip" {
  description = "Assign a public IP to the Fargate task. Required if the task runs in a public subnet without a NAT."
  type        = bool
  default     = false
}

variable "efs_transition_to_ia" {
  description = "EFS lifecycle policy — when to transition idle files to Infrequent Access (cost saver)."
  type        = string
  default     = "AFTER_30_DAYS"
}
