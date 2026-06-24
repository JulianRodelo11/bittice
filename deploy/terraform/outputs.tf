output "public_ip" {
  description = "Elastic IP de la instancia"
  value       = aws_eip.bittice.public_ip
}

output "instance_id" {
  value = aws_instance.bittice.id
}

output "ssh_command" {
  description = "Conectarse a la instancia"
  value       = "ssh -i <tu-clave-privada> ubuntu@${aws_eip.bittice.public_ip}"
}

output "admin_tunnel_hint" {
  description = "Admin API is not on the public internet — use SSH port forward"
  value       = "ssh -L 8080:127.0.0.1:8080 -i <key> ubuntu@${aws_eip.bittice.public_ip}  # then http://127.0.0.1:8080"
}
