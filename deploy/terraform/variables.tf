variable "aws_region" {
  description = "AWS region"
  default     = "us-east-1"
}

variable "instance_type" {
  description = "EC2 instance type"
  default     = "t3.micro"
}

variable "ssh_public_key" {
  description = "SSH public key content for EC2 access (e.g. contents of ~/.ssh/id_rsa.pub)"
  type        = string
}

variable "allowed_ssh_cidr" {
  description = "CIDR allowed for SSH — restrict to your IP in production"
  default     = "0.0.0.0/0"
}

variable "allowed_admin_cidr" {
  description = "CIDR allowed for port 8080 (admin API) — VPC private range by default, never public"
  default     = "172.31.0.0/16"
}
variable "app_name" {
  description = "Used for tagging and resource naming"
  default     = "bittice"
}

variable "data_volume_size_gb" {
  description = "Root EBS volume size in GB"
  default     = 20
}

# ── RDS-aware placement (multi-tenant) ──────────────────────────────────────
# When the user's RDS lives in their own AWS account, the wizard discovers the
# RDS's VPC/subnet/SG and passes them here. Bittice is then placed in the same
# VPC, eliminating the VPN entirely. When empty (default), Bittice falls back
# to the AWS account's default VPC and the user must arrange connectivity
# (OpenVPN sidecar, public RDS, etc.).

variable "target_vpc_id" {
  description = "VPC to place Bittice in. Empty = use default VPC."
  type        = string
  default     = ""
}

variable "target_subnet_id" {
  description = "Subnet for the Bittice EC2 (must be public — needs IGW for SSH/REST/gRPC ingress)."
  type        = string
  default     = ""
}

variable "target_rds_security_group_ids" {
  description = "Security groups of target RDSes (one entry per RDS Bittice mirrors). For each ID, an inbound rule on 3306 from the Bittice SG is added so CDC can reach MySQL."
  type        = list(string)
  default     = []
}

variable "rds_port" {
  description = "Port to open from Bittice EC2 toward the RDS SG."
  type        = number
  default     = 3306
}
