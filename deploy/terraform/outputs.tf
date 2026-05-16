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
