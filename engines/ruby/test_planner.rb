# frozen_string_literal: true
require 'minitest/autorun'
require_relative 'preppydb_planner'

class PlannerTest < Minitest::Test
  def request
    { 'version' => 1, 'collection' => 'users', 'limit' => 25,
      'filter' => { 'field' => 'profile.level', 'op' => 'gte', 'value' => 10 },
      'indexes' => [{ 'name' => 'level', 'field' => 'profile.level', 'sparse' => true }] }
  end

  def test_range_and_scan_are_deterministic
    assert_equal 'full_scan', PreppyDB::Planner.plan(request)['strategy']
    input = request.merge('indexes' => [])
    assert_equal 'full_scan', PreppyDB::Planner.plan(input)['strategy']
    assert_equal PreppyDB::Planner.plan(request), PreppyDB::Planner.plan(request)
  end

  def test_equality_and_recheck
    query = request
    query['filter']['op'] = 'eq'
    query['filter']['value'] = '10'
    result = PreppyDB::Planner.plan(query)
    assert_equal 'index_equality', result['strategy']
    assert_equal true, result['recheck']
  end

  def test_rejects_untrusted_input
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.parse_and_plan(' ' * 65_537) }
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.parse_and_plan('[' * 30 + ']' * 30) }
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.plan(request.merge('limit' => -1)) }
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.plan(request.merge('version' => 2)) }
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.plan(request.merge('indexes' => [{}])) }
    assert_raises(PreppyDB::Planner::InvalidQuery) { PreppyDB::Planner.plan(request.merge('filter' => false)) }
  end
end
