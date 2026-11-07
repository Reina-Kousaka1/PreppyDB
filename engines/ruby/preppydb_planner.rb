# frozen_string_literal: true

# Deterministic document-query planning. Plans are untrusted until validated by Rust.
require 'json'

module PreppyDB
  module Planner
    VERSION = 1
    MAX_INPUT_BYTES = 65_536
    MAX_INDEXES = 256
    MAX_LIMIT = 1_000
    IDENTIFIER = /\A[A-Za-z_][A-Za-z0-9_.-]{0,127}\z/
    OPERATORS = %w[eq gte lte].freeze

    class InvalidQuery < StandardError; end

    def self.identifier(value, kind)
      raise InvalidQuery, "invalid #{kind}" unless value.is_a?(String) && IDENTIFIER.match?(value)
      value
    end

    def self.plan(request)
      raise InvalidQuery, 'request must be an object' unless request.is_a?(Hash)
      raise InvalidQuery, 'unsupported request version' unless request['version'] == VERSION
      collection = identifier(request['collection'], 'collection')
      limit = request.fetch('limit', 100)
      raise InvalidQuery, 'invalid limit' unless limit.is_a?(Integer) && limit.between?(1, MAX_LIMIT)
      indexes = request.fetch('indexes', [])
      raise InvalidQuery, 'invalid index metadata' unless indexes.is_a?(Array) && indexes.length <= MAX_INDEXES
      indexes.each do |index|
        raise InvalidQuery, 'invalid index metadata' unless index.is_a?(Hash)
        identifier(index['name'], 'index name')
        identifier(index['field'], 'index field')
        raise InvalidQuery, 'invalid sparse metadata' unless [true, false].include?(index['sparse'])
      end
      filter = request['filter']
      unless filter.nil?
        raise InvalidQuery, 'invalid filter' unless filter.is_a?(Hash)
        field = identifier(filter['field'], 'field')
        operator = filter['op']
        value = filter['value']
        raise InvalidQuery, 'invalid operator' unless OPERATORS.include?(operator)
        raise InvalidQuery, 'missing value' unless filter.key?('value')
        raise InvalidQuery, 'unsupported value' unless value.nil? || value == true || value == false || value.is_a?(String) || value.is_a?(Integer) || (value.is_a?(Float) && value.finite?)
        # Restrict candidates to comparisons the Rust validator can currently prove safe.
        # Numeric/null equality and all ranges require further index-semantic qualification.
        if operator == 'eq' && (value.is_a?(String) || value == true || value == false)
          index = indexes.select { |item| item['field'] == field }.min_by { |item| item['name'] }
        end
      end
      strategy = if index
                   filter['op'] == 'eq' ? 'index_equality' : 'index_range'
                 else
                   'full_scan'
                 end
      { 'version' => VERSION, 'collection' => collection, 'filter' => filter,
        'limit' => limit, 'strategy' => strategy, 'index' => index && index['name'],
        'cost_hint' => index ? 'indexed_candidate' : 'scan', 'recheck' => true }
    end

    def self.parse_and_plan(bytes)
      raise InvalidQuery, 'request too large' if bytes.bytesize > MAX_INPUT_BYTES
      plan(JSON.parse(bytes, max_nesting: 16, allow_nan: false))
    rescue JSON::ParserError, JSON::NestingError => e
      raise InvalidQuery, "invalid JSON: #{e.class}"
    end
  end
end

if $PROGRAM_NAME == __FILE__
  begin
    input = $stdin.read(PreppyDB::Planner::MAX_INPUT_BYTES + 1)
    $stdout.write(JSON.generate(PreppyDB::Planner.parse_and_plan(input)))
  rescue PreppyDB::Planner::InvalidQuery => e
    $stderr.write("invalid query: #{e.message}\n")
    exit 2
  end
end
