#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

files = Dir[File.expand_path("../.github/**/*.yml", __dir__)].sort
abort "no YAML files found" if files.empty?
files.each do |file|
  YAML.parse_file(file)
  puts "YAML OK: #{file}"
end
