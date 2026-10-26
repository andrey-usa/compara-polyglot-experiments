@auth
Feature: Signing in
  A shopper signs in with a password.

  Background:
    Given I open "/login"

  @smoke
  Scenario: The right password signs in
    When I sign in with "hunter22"
    Then I see "Signed in"
    And the URL ends with "/account"
    But I don't see "wrong password"

  Scenario Outline: A wrong password says so
    When I fill "Password" with "<pw>"
    And I click "Sign in"
    Then I see "wrong password"

    Examples:
      | pw    |
      | nope  |
      | 12345 |
