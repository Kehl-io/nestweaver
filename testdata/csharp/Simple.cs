using System;
using System.Collections.Generic;

namespace MyApp.Models
{
    public interface IGreeter
    {
        string Greet(string name);
    }

    public enum Priority
    {
        Low,
        Medium,
        High
    }

    public class SimpleGreeter : IGreeter
    {
        public string Greet(string name)
        {
            return $"Hello, {name}!";
        }

        private void LogGreeting(string message)
        {
            Console.WriteLine(message);
        }
    }

    public static class Program
    {
        public static void Main(string[] args)
        {
            var greeter = new SimpleGreeter();
            var result = greeter.Greet("World");
            Console.WriteLine(result);
        }
    }
}
