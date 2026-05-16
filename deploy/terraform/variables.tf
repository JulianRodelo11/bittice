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
