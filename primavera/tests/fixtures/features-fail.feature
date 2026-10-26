Feature: A failing feature
  Scenario: The title is wrong
    Given I open "/login"
    Then the title is "Nope"
    And I see "never reached"
